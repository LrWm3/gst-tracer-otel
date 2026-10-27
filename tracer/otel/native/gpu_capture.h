/* Internal controller shared by the CUPTI adapter and its CUDA-free tests. */
#ifndef MARS_GPU_CAPTURE_H
#define MARS_GPU_CAPTURE_H
#include "gpu_adapter.h"
int mars_capture_configure(uint32_t window_ms, uint32_t period_ms);
/* Collection must already be enabled. set_active is called only by the
 * controller, never by a CUDA callback. It must serialize with adapter flush. */
int mars_capture_start(int (*set_active)(int));
int mars_capture_stop(void);
int mars_capture_launch(void);
int mars_capture_stats(MarsGpuCaptureStats *stats);
#endif
