/* Experimental CUDA 11.4 adapter. Build against the matching CUPTI headers
 * and runtime. All Rust/pprof/HTTP handling is in gpuprofiles.rs.
 * CUPTI's global buffer callbacks and this DSO stay resident until exit.
 */
#define _POSIX_C_SOURCE 200809L
#include "gpu_adapter.h"
#include <cupti.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

/* New SDKs change record layouts. Fail the build instead of interpreting a
 * different layout as Kernel6. Extend with a tested version-specific decoder. */
#if CUDA_VERSION != 11040
#error "This experimental adapter currently requires CUDA 11.4 headers/runtime"
#endif

#define BUFFER_COUNT 8
#define BUFFER_SIZE (256 * 1024)
static struct { unsigned char *data; atomic_bool busy; } buffers[BUFFER_COUNT];
static MarsGpuReceive receiver;
static CUpti_SubscriberHandle subscriber;
static int started;

static void CUPTIAPI request_buffer(uint8_t **buffer, size_t *size, size_t *max_records) {
    *buffer = NULL; *size = 0; *max_records = 0;
    for (unsigned i = 0; i < BUFFER_COUNT; ++i) {
        _Bool expected = 0;
        if (atomic_compare_exchange_strong(&buffers[i].busy, &expected, 1)) {
            *buffer = buffers[i].data; *size = BUFFER_SIZE; return;
        }
    }
    /* CUPTI counts records it cannot place; report those in completion. */
}

static void CUPTIAPI complete_buffer(CUcontext context, uint32_t stream,
    uint8_t *buffer, size_t size, size_t valid) {
    (void)size;
    /* CUPTI uses *record as the iterator cursor; the first call requires NULL. */
    CUpti_Activity *record = NULL;
    CUptiResult result = CUPTI_ERROR_MAX_LIMIT_REACHED;
    while (valid && (result = cuptiActivityGetNextRecord(buffer, valid, &record)) == CUPTI_SUCCESS) {
        if (record->kind != CUPTI_ACTIVITY_KIND_CONCURRENT_KERNEL) continue;
        const CUpti_ActivityKernel6 *kernel = (const CUpti_ActivityKernel6 *)record;
        MarsGpuEvent event = { .kind = 2, .correlation = kernel->correlationId,
            .start = kernel->start, .end = kernel->end,
            .device = kernel->deviceId, .stream = kernel->streamId };
        if (kernel->name) {
            size_t len = strnlen(kernel->name, sizeof(event.name) - 1);
            memcpy(event.name, kernel->name, len);
        }
        receiver(&event);
    }
    if (result != CUPTI_ERROR_MAX_LIMIT_REACHED) {
        MarsGpuEvent error = { .kind = 3, .count = 1 }; receiver(&error);
    }
    size_t dropped = 0;
    if (cuptiActivityGetNumDroppedRecords(context, stream, &dropped) == CUPTI_SUCCESS && dropped) {
        MarsGpuEvent lost = { .kind = 3, .count = dropped }; receiver(&lost);
    }
    for (unsigned i = 0; i < BUFFER_COUNT; ++i) {
        if (buffers[i].data == buffer) { atomic_store(&buffers[i].busy, 0); break; }
    }
}

static void CUPTIAPI on_api(void *userdata, CUpti_CallbackDomain domain,
    CUpti_CallbackId id, const void *data) {
    (void)userdata; (void)id;
    if (domain != CUPTI_CB_DOMAIN_DRIVER_API) return;
    const CUpti_CallbackData *callback = data;
    if (callback->callbackSite != CUPTI_API_ENTER) return;
    /* Driver launch callbacks cover runtime API launches as well. Enable only
     * the tested ordinary kernel APIs; graph attribution is a later extension. */
    struct timespec now;
    if (clock_gettime(CLOCK_REALTIME, &now) != 0) return;
    MarsGpuEvent launch = { .kind = 1, .correlation = callback->correlationId,
        .start = (uint64_t)now.tv_sec * 1000000000ULL + (uint64_t)now.tv_nsec };
    receiver(&launch);
}

int mars_gpu_start_v1(MarsGpuReceive receive) {
    if (started || !receive) return -1;
    uint32_t version = 0;
    CUptiResult result = cuptiGetVersion(&version);
    if (result != CUPTI_SUCCESS) return (int)result;
    if (version != CUPTI_API_VERSION) return -2;
    /* Bound CUPTI's per-context timestamp storage before CUDA initialization.
     * CUDA 11.4 defaults to pinned host storage despite the DEVICE_BUFFER names.
     * CUPTI may round these allocations up; other internal memory is separate
     * from these limits and our fixed 2 MiB activity-buffer pool. */
    size_t bytes = 1024 * 1024, pools = 4, preallocate = 2, value_size = sizeof(size_t);
    result = cuptiActivitySetAttribute(CUPTI_ACTIVITY_ATTR_DEVICE_BUFFER_SIZE, &value_size, &bytes);
    if (result != CUPTI_SUCCESS) return (int)result;
    result = cuptiActivitySetAttribute(CUPTI_ACTIVITY_ATTR_DEVICE_BUFFER_POOL_LIMIT, &value_size, &pools);
    if (result != CUPTI_SUCCESS) return (int)result;
    result = cuptiActivitySetAttribute(CUPTI_ACTIVITY_ATTR_DEVICE_BUFFER_PRE_ALLOCATE_VALUE, &value_size, &preallocate);
    if (result != CUPTI_SUCCESS) return (int)result;
    receiver = receive;
    for (unsigned i = 0; i < BUFFER_COUNT; ++i) {
        if (posix_memalign((void **)&buffers[i].data, 8, BUFFER_SIZE) != 0) return -3;
    }
    result = cuptiSubscribe(&subscriber, on_api, NULL);
    if (result != CUPTI_SUCCESS) return (int)result;
    result = cuptiActivityRegisterCallbacks(request_buffer, complete_buffer);
    if (result != CUPTI_SUCCESS) goto fail;
    /* Launch ownership comes from the selected API callbacks below. Driver
     * activity records repeat that correlation ID but are never consumed;
     * collect only the kernel records that supply execution durations. */
    result = cuptiActivityEnable(CUPTI_ACTIVITY_KIND_CONCURRENT_KERNEL);
    if (result != CUPTI_SUCCESS) goto fail;
    const CUpti_CallbackId callbacks[] = {
        CUPTI_DRIVER_TRACE_CBID_cuLaunchKernel,
        CUPTI_DRIVER_TRACE_CBID_cuLaunchKernel_ptsz,
        CUPTI_DRIVER_TRACE_CBID_cuLaunchCooperativeKernel,
        CUPTI_DRIVER_TRACE_CBID_cuLaunchCooperativeKernel_ptsz
    };
    for (unsigned i = 0; i < sizeof(callbacks) / sizeof(callbacks[0]); ++i) {
        result = cuptiEnableCallback(1, subscriber, CUPTI_CB_DOMAIN_DRIVER_API, callbacks[i]);
        if (result != CUPTI_SUCCESS) goto fail;
    }
    started = 1;
    return 0;
fail:
    cuptiUnsubscribe(subscriber);
    cuptiActivityDisable(CUPTI_ACTIVITY_KIND_CONCURRENT_KERNEL);
    return (int)result;
}

int mars_gpu_flush_v1(uint32_t stop) {
    if (!started) return 0;
    /* Never synchronize the device or flush from a CUPTI callback. The caller
     * must finish application GPU work before requesting an exact final drain. */
    CUptiResult result = cuptiActivityFlushAll(0);
    if (stop) {
        CUptiResult status = cuptiUnsubscribe(subscriber);
        if (result == CUPTI_SUCCESS) result = status;
        status = cuptiActivityDisable(CUPTI_ACTIVITY_KIND_CONCURRENT_KERNEL);
        if (result == CUPTI_SUCCESS) result = status;
        started = 0;
    }
    return (int)result;
}
