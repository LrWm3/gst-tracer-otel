/* Diagnostic only: fresh-process CUDA/CUPTI memory phases, without Rust/HTTP.
 * Run: gpu_memory [none|/absolute/adapter.so] [load-only]
 * Each invocation creates one context, compiles one tiny kernel and verifies
 * 2001 launches. It never restarts the adapter or changes clock/power policy. */
#define _GNU_SOURCE
#include "gpu_adapter.h"
#include <cuda.h>
#include <nvrtc.h>
#include <dlfcn.h>
#include <malloc.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define CHECK(call) do { int status = (int)(call); if (status) { \
    fprintf(stderr, "%s failed: %d\n", #call, status); return 1; } } while (0)
#define SLOTS 16384
static atomic_uint correlations[SLOTS];
static atomic_uint launches, kernels, unmatched, collisions, dropped;

static void receive(const MarsGpuEvent *event) {
    unsigned index = event->correlation % SLOTS;
    if (event->kind == 1) {
        atomic_fetch_add(&launches, 1);
        if (atomic_exchange(&correlations[index], event->correlation))
            atomic_fetch_add(&collisions, 1);
    } else if (event->kind == 2) {
        atomic_fetch_add(&kernels, 1);
        unsigned expected = event->correlation;
        if (!expected || !atomic_compare_exchange_strong(&correlations[index], &expected, 0))
            atomic_fetch_add(&unmatched, 1);
    } else if (event->kind == 3) {
        atomic_fetch_add(&dropped, (unsigned)event->count);
    }
}

static unsigned long field(const char *file, const char *key) {
    FILE *stream = fopen(file, "r");
    if (!stream) { perror(file); exit(1); }
    char line[512]; unsigned long value = 0;
    while (fgets(line, sizeof(line), stream)) {
        if (!strncmp(line, key, strlen(key))) {
            value = strtoul(line + strlen(key), NULL, 10);
            break;
        }
    }
    fclose(stream);
    return value;
}

static void phase(const char *name) {
    struct timespec cpu;
    clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &cpu);
    printf("{\"phase\":\"%s\",\"rss_kib\":%lu,\"pss_kib\":%lu,"
           "\"private_dirty_kib\":%lu,\"locked_kib\":%lu,\"peak_kib\":%lu,"
           "\"cpu_ns\":%llu,\"launches\":%u,\"kernels\":%u,"
           "\"unmatched\":%u,\"collisions\":%u,\"dropped\":%u}\n",
           name, field("/proc/self/smaps_rollup", "Rss:"),
           field("/proc/self/smaps_rollup", "Pss:"),
           field("/proc/self/smaps_rollup", "Private_Dirty:"),
           field("/proc/self/smaps_rollup", "Locked:"),
           field("/proc/self/status", "VmHWM:"),
           (unsigned long long)cpu.tv_sec * 1000000000ULL + (unsigned long long)cpu.tv_nsec,
           atomic_load(&launches), atomic_load(&kernels), atomic_load(&unmatched),
           atomic_load(&collisions), atomic_load(&dropped));
    fflush(stdout);
    /* Optional private evidence: preserve mapping attribution at each phase. */
    const char *directory = getenv("MARS_GPU_MEMORY_OUTPUT");
    if (directory) {
        char path[4096];
        int length = snprintf(path, sizeof(path), "%s/%s.smaps", directory, name);
        if (length < 0 || (size_t)length >= sizeof(path)) exit(1);
        FILE *input = fopen("/proc/self/smaps", "r"), *output = fopen(path, "w");
        if (!input || !output) { perror("memory snapshot"); exit(1); }
        char bytes[16384]; size_t count;
        while ((count = fread(bytes, 1, sizeof(bytes), input)))
            if (fwrite(bytes, 1, count, output) != count) exit(1);
        fclose(input); fclose(output);
    }
}

int main(int argc, char **argv) {
    if (argc < 2 || argc > 3 || (argc == 3 && strcmp(argv[2], "load-only"))) {
        fprintf(stderr, "usage: gpu_memory none|/absolute/adapter.so [load-only]\n");
        return 1;
    }
    phase("initial");
    int (*flush)(uint32_t) = NULL;
    if (strcmp(argv[1], "none")) {
        void *library = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL);
        if (!library) { fprintf(stderr, "%s\n", dlerror()); return 1; }
        int (*start)(MarsGpuReceive) = dlsym(library, "mars_gpu_start_v1");
        flush = dlsym(library, "mars_gpu_flush_v1");
        if (!start || !flush) return 1;
        phase("adapter_loaded");
        if (argc == 3) flush = NULL;
        else { CHECK(start(receive)); phase("adapter_started"); }
    }
    CHECK(cuInit(0)); phase("cuda_initialized");
    CUdevice device; CUcontext context; CUmodule module; CUfunction kernel;
    CHECK(cuDeviceGet(&device, 0));
    CHECK(cuCtxCreate(&context, 0, device)); phase("context_created");
    int major, minor;
    CHECK(cuDeviceGetAttribute(&major, CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MAJOR, device));
    CHECK(cuDeviceGetAttribute(&minor, CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MINOR, device));
    const char *source = "extern \"C\" __global__ void count_kernel(unsigned *out) { atomicAdd(out, 1); }";
    nvrtcProgram program;
    CHECK(nvrtcCreateProgram(&program, source, "count.cu", 0, NULL, NULL));
    char arch[64]; snprintf(arch, sizeof(arch), "--gpu-architecture=compute_%d%d", major, minor);
    const char *options[] = {arch};
    CHECK(nvrtcCompileProgram(program, 1, options));
    size_t size; CHECK(nvrtcGetPTXSize(program, &size));
    char *ptx = malloc(size); if (!ptx) return 1;
    CHECK(nvrtcGetPTX(program, ptx));
    CHECK(nvrtcDestroyProgram(&program)); phase("nvrtc_compiled");
    CHECK(cuModuleLoadData(&module, ptx)); free(ptx); phase("module_loaded");
    CHECK(cuModuleGetFunction(&kernel, module, "count_kernel"));
    CUdeviceptr output; CHECK(cuMemAlloc(&output, sizeof(unsigned)));
    CHECK(cuMemsetD32(output, 0, 1));
    void *args[] = {&output};
    CHECK(cuLaunchKernel(kernel, 1, 1, 1, 1, 1, 1, 0, NULL, args, NULL));
    CHECK(cuCtxSynchronize()); phase("first_kernel");
    for (unsigned i = 0; i < 2000; ++i)
        CHECK(cuLaunchKernel(kernel, 1, 1, 1, 1, 1, 1, 0, NULL, args, NULL));
    CHECK(cuCtxSynchronize()); phase("sustained_kernels");
    unsigned actual = 0; CHECK(cuMemcpyDtoH(&actual, output, sizeof(actual)));
    if (actual != 2001) return 1;
    if (flush) CHECK(flush(0));
    phase("flushed");
    CHECK(cuMemFree(output)); CHECK(cuModuleUnload(module)); CHECK(cuCtxDestroy(context));
    phase("context_destroyed");
    if (flush) {
        CHECK(flush(1));
        if (atomic_load(&launches) != 2001 || atomic_load(&kernels) != 2001 ||
            atomic_load(&unmatched) || atomic_load(&collisions) || atomic_load(&dropped)) return 1;
    }
    phase("stopped");
    sleep(1); phase("idle");
    /* Diagnostic only, never called by the production profiler. */
    malloc_trim(0); phase("allocator_trimmed");
    return 0;
}
