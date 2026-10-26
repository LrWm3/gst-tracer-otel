/* Standalone demo workload: two streams launch identical named kernels with
 * different durations. The host callback delays execution beyond the span.
 * No processor application or model files are required. */
#define _POSIX_C_SOURCE 200809L
#include <cuda.h>
#include <nvrtc.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

static CUcontext context;
static CUmodule module;
static CUfunction kernels[3];
static CUstream streams[2];
static CUdeviceptr outputs[2];
static int clock_khz;
#define CUDA(call) do { CUresult e = (call); if (e != CUDA_SUCCESS) { fprintf(stderr, "CUDA demo error %d at %s\n", e, #call); return (int)e; } } while (0)
#define NVRTC(call) do { nvrtcResult e = (call); if (e != NVRTC_SUCCESS) { fprintf(stderr, "NVRTC demo error: %s\n", nvrtcGetErrorString(e)); return -(int)e; } } while (0)

int mars_workload_initialize(void) {
    CUDA(cuInit(0));
    CUdevice device;
    CUDA(cuDeviceGet(&device, 0));
    CUDA(cuCtxCreate(&context, 0, device));
    CUDA(cuDeviceGetAttribute(&clock_khz, CU_DEVICE_ATTRIBUTE_CLOCK_RATE, device));
    int major, minor;
    CUDA(cuDeviceGetAttribute(&major, CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MAJOR, device));
    CUDA(cuDeviceGetAttribute(&minor, CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MINOR, device));
    const char *source =
        "__device__ void work(unsigned long long ticks, unsigned *out) { unsigned long long start = clock64(); while (clock64() - start < ticks) { } if (!threadIdx.x && !blockIdx.x) *out = (unsigned)clock64(); }\n"
        "extern \"C\" __global__ void resize_kernel(unsigned long long t, unsigned *o) { work(t,o); }\n"
        "extern \"C\" __global__ void normalize_kernel(unsigned long long t, unsigned *o) { work(t,o); }\n"
        "extern \"C\" __global__ void convert_kernel(unsigned long long t, unsigned *o) { work(t,o); }\n";
    nvrtcProgram program;
    NVRTC(nvrtcCreateProgram(&program, source, "mars_gpu_demo.cu", 0, NULL, NULL));
    char arch[64]; snprintf(arch, sizeof(arch), "--gpu-architecture=compute_%d%d", major, minor);
    const char *options[] = { arch };
    NVRTC(nvrtcCompileProgram(program, 1, options));
    size_t size;
    NVRTC(nvrtcGetPTXSize(program, &size));
    char *ptx = malloc(size);
    if (!ptx) return -100;
    NVRTC(nvrtcGetPTX(program, ptx));
    CUDA(cuModuleLoadData(&module, ptx));
    free(ptx);
    NVRTC(nvrtcDestroyProgram(&program));
    const char *names[] = { "resize_kernel", "normalize_kernel", "convert_kernel" };
    for (unsigned i = 0; i < 3; ++i) CUDA(cuModuleGetFunction(&kernels[i], module, names[i]));
    for (unsigned i = 0; i < 2; ++i) {
        CUDA(cuStreamCreate(&streams[i], CU_STREAM_NON_BLOCKING));
        CUDA(cuMemAlloc(&outputs[i], sizeof(unsigned)));
    }
    return 0;
}
static void CUDA_CB delay(void *userdata) {
    (void)userdata;
    struct timespec remaining = { .tv_sec = 0, .tv_nsec = 100000000 };
    while (nanosleep(&remaining, &remaining) != 0) { }
}
int mars_workload_launch(uint32_t stage) {
    if (stage >= 2) return -100;
    CUDA(cuCtxSetCurrent(context));
    CUDA(cuLaunchHostFunc(streams[stage], delay, NULL));
    for (unsigned i = 0; i < 3; ++i) {
        unsigned long long ticks = (unsigned long long)clock_khz * (stage ? 20 : 5) * (i + 1);
        void *args[] = { &ticks, &outputs[stage] };
        CUDA(cuLaunchKernel(kernels[i], 1, 1, 1, 128, 1, 1, 0, streams[stage], args, NULL));
    }
    return 0;
}
int mars_workload_finish(void) {
    CUDA(cuCtxSetCurrent(context));
    CUDA(cuCtxSynchronize());
    for (unsigned i = 0; i < 2; ++i) { CUDA(cuMemFree(outputs[i])); CUDA(cuStreamDestroy(streams[i])); }
    CUDA(cuModuleUnload(module));
    CUDA(cuCtxDestroy(context));
    return 0;
}
