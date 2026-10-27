/* Controller lifecycle tests: no CUDA installation or GPU required. */
#define _POSIX_C_SOURCE 200809L
#include "gpu_capture.h"
#include <assert.h>
#include <stdatomic.h>
#include <string.h>
#include <time.h>
static atomic_int transitions;
static int fail;
static uint64_t now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (uint64_t)t.tv_sec * 1000000000ULL + t.tv_nsec;
}
static void pause_ms(void) {
    struct timespec t = { .tv_nsec = 1000000 };
    nanosleep(&t, NULL);
}
static int change(int active) {
    (void)active;
    /* Simulate a slow control operation: launch callbacks must stay independent. */
    for (unsigned i = 0; i < 10; ++i) pause_ms();
    atomic_fetch_add(&transitions, 1);
    return fail ? -7 : 0;
}
int main(int argc, char **argv) {
    assert(argc == 2);
    int continuous = !strcmp(argv[1], "continuous");
    int full = !strcmp(argv[1], "full");
    int long_period = !strcmp(argv[1], "long");
    fail = !strcmp(argv[1], "failure");
    assert(mars_capture_configure(1, 100) != 0);
    assert(mars_capture_configure(101, 100) != 0);
    assert(mars_capture_configure(0, 3600001) != 0);
    assert(mars_capture_configure(continuous ? 0 : 30,
        full ? 30 : (long_period ? 3600000 : 100)) == 0);
    assert(mars_capture_start(change) == 0);
    assert(mars_capture_configure(0, 100) != 0);
    assert(mars_capture_start(change) != 0);
    assert(mars_capture_launch());
    uint64_t start = now(), calls = 1;
    while (now() - start < 350000000ULL) { mars_capture_launch(); ++calls; pause_ms(); }
    MarsGpuCaptureStats s;
    assert(mars_capture_stats(&s) == 0);
    assert(s.captured_launches + s.skipped_launches == calls);
    assert(s.active_ns <= s.elapsed_ns);
    assert(s.control_failures == (uint64_t)fail);
    if (continuous || full) {
        assert(!s.skipped_launches && s.windows == 1);
        assert(s.active_ns == s.elapsed_ns);
        assert(!atomic_load(&transitions));
    } else {
        assert(s.skipped_launches && atomic_load(&transitions));
        if (!long_period && !fail) assert(s.windows >= 3);
    }
    start = now();
    assert(mars_capture_stop() == (fail ? -7 : 0));
    assert(now() - start < 500000000ULL);
    assert(!mars_capture_launch());
    assert(mars_capture_stats(&s) == 0 && !s.active);
    MarsGpuCaptureStats after;
    pause_ms();
    assert(mars_capture_stats(&after) == 0);
    assert(!memcmp(&s, &after, sizeof(s)));
    assert(mars_capture_stop() == (fail ? -7 : 0));
    return 0;
}
