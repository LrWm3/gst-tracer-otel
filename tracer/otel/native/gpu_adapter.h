/* Mars GPU adapter ABI v1. No CUDA types cross this boundary. */
#ifndef MARS_GPU_ADAPTER_H
#define MARS_GPU_ADAPTER_H
#include <stdint.h>
typedef struct {
    uint32_t kind; /* 1: host launch, 2: completed kernel, 3: lost records */
    uint32_t correlation;
    uint64_t start; /* launch: Unix ns; kernel: CUPTI device timestamp */
    uint64_t end;
    uint32_t device;
    uint32_t stream;
    uint64_t count;
    char name[256];
} MarsGpuEvent;
_Static_assert(sizeof(MarsGpuEvent) == 296, "Mars GPU ABI v1 layout");
typedef void (*MarsGpuReceive)(const MarsGpuEvent *);
int mars_gpu_start_v1(MarsGpuReceive receive);
int mars_gpu_flush_v1(uint32_t stop);

/* Optional capture-window extension. Configure before start; zero window means
 * continuous capture. Older v1 adapters remain usable for continuous capture.
 * Counters describe host collection coverage, never scaled GPU utilization. */
typedef struct {
    uint64_t elapsed_ns;
    uint64_t active_ns;
    uint64_t windows;
    uint64_t captured_launches;
    uint64_t skipped_launches;
    uint64_t active;
    uint64_t control_failures;
} MarsGpuCaptureStats;
_Static_assert(sizeof(MarsGpuCaptureStats) == 56, "Mars GPU capture stats v1 layout");
int mars_gpu_configure_capture_v1(uint32_t window_ms, uint32_t period_ms);
int mars_gpu_capture_stats_v1(MarsGpuCaptureStats *stats);
#endif
