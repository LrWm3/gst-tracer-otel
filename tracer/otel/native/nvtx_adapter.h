/* Mars NVTX adapter ABI v1. No NVTX/CUDA headers cross this boundary. */
#ifndef MARS_NVTX_ADAPTER_H
#define MARS_NVTX_ADAPTER_H
#include <stdint.h>
/* Events run synchronously on the NVTX caller. Names are borrowed until return.
 * Return values are opaque, process-unique domain IDs (create), or range depth.
 * 1=create ASCII domain, 2=destroy, 3=push, 4=pop. Zero is the unknown domain.
 */
typedef uintptr_t (*MarsNvtxReceive)(uint32_t kind, uintptr_t domain, const char *name);
int mars_nvtx_start_v1(MarsNvtxReceive receive);
#endif
