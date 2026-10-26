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
#endif
