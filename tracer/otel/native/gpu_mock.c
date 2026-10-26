/* Test-only adapter: delivers kernels after their host scopes have ended. */
#define _POSIX_C_SOURCE 200809L
#include "gpu_adapter.h"
#include <pthread.h>
#include <string.h>
#include <time.h>
static MarsGpuReceive receiver;
static MarsGpuEvent kernels[256];
static unsigned next, pending;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
int mars_gpu_start_v1(MarsGpuReceive receive) { receiver = receive; return 0; }
void mars_gpu_mock_launch_v1(void) {
    pthread_mutex_lock(&lock);
    struct timespec now;
    clock_gettime(CLOCK_REALTIME, &now);
    MarsGpuEvent launch = { .kind = 1, .correlation = ++next,
        .start = (uint64_t)now.tv_sec * 1000000000ULL + (uint64_t)now.tv_nsec };
    receiver(&launch);
    if (pending < 256) {
        MarsGpuEvent kernel = { .kind = 2, .correlation = launch.correlation, .start = 1, .end = 10000001 };
        strcpy(kernel.name, "same_kernel");
        kernels[pending++] = kernel;
    }
    pthread_mutex_unlock(&lock);
}
int mars_gpu_flush_v1(uint32_t stop) {
    (void)stop;
    pthread_mutex_lock(&lock);
    for (unsigned i = 0; i < pending; ++i) receiver(&kernels[i]);
    pending = 0;
    pthread_mutex_unlock(&lock);
    return 0;
}
