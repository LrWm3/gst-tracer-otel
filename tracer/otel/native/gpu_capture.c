/* Periodic collection control, independent of report encoding and HTTP waits.
 * Launch callbacks never lock, allocate, wait, or call a CUDA/CUPTI API here.
 * Keep this object resident alongside CUPTI's process-global callbacks. */
#define _POSIX_C_SOURCE 200809L
#include "gpu_capture.h"
#include <pthread.h>
#include <stdatomic.h>
#include <time.h>

static pthread_mutex_t state_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t wakeup;
static pthread_t controller;
static int started, threaded, stopping, failure;
static uint64_t window_ns, period_ns, accumulated_ns, active_since, windows;
static uint64_t stopped_at;
static int (*change_capture)(int);
static atomic_bool enabled, accepting;
static atomic_uint_fast64_t first_launch, captured, skipped;

static uint64_t now_ns(void) {
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (uint64_t)now.tv_sec * 1000000000ULL + (uint64_t)now.tv_nsec;
}

int mars_capture_configure(uint32_t window_ms, uint32_t period_ms) {
    if (started || stopping || window_ms > 3600000 || period_ms > 3600000 ||
        (window_ms && (window_ms < 10 || period_ms < window_ms))) return -1;
    /* A full-period window is continuous and needs no controller thread. */
    window_ns = window_ms == period_ms ? 0 : (uint64_t)window_ms * 1000000ULL;
    period_ns = (uint64_t)period_ms * 1000000ULL;
    return 0;
}

static void account_active(uint64_t now, uint64_t epoch) {
    if (epoch && atomic_load_explicit(&enabled, memory_order_relaxed)) {
        uint64_t since = active_since ? active_since : epoch;
        if (now >= since) accumulated_ns += now - since;
    }
}

static void *run(void *unused) {
    (void)unused;
    pthread_mutex_lock(&state_lock);
    while (!stopping) {
        uint64_t epoch = atomic_load_explicit(&first_launch, memory_order_relaxed);
        uint64_t now = now_ns();
        uint64_t deadline = now + 10000000ULL;
        if (epoch) {
            uint64_t elapsed = now - epoch, phase = elapsed % period_ns;
            int wanted = phase < window_ns;
            if (wanted != atomic_load_explicit(&enabled, memory_order_relaxed)) {
                int status;
                if (wanted) {
                    /* Own launches before enabling activity. At a boundary an
                     * extra owner is safer than losing a late kernel's owner. */
                    active_since = now_ns();
                    atomic_store_explicit(&enabled, 1, memory_order_relaxed);
                    status = change_capture(1);
                    ++windows;
                } else {
                    status = change_capture(0);
                    account_active(now_ns(), epoch);
                    atomic_store_explicit(&enabled, 0, memory_order_relaxed);
                }
                if (status) {
                    failure = status;
                    break;
                }
            }
            deadline = epoch + (elapsed / period_ns) * period_ns +
                (wanted ? window_ns : period_ns);
        }
        struct timespec absolute = { .tv_sec = deadline / 1000000000ULL,
            .tv_nsec = deadline % 1000000000ULL };
        pthread_cond_timedwait(&wakeup, &state_lock, &absolute);
    }
    pthread_mutex_unlock(&state_lock);
    return NULL;
}

int mars_capture_start(int (*set_active)(int)) {
    if (started || stopping || !set_active) return -1;
    change_capture = set_active;
    atomic_store_explicit(&enabled, 1, memory_order_relaxed);
    if (window_ns) {
        pthread_condattr_t attributes;
        int status = pthread_condattr_init(&attributes);
        if (status) return status;
        status = pthread_condattr_setclock(&attributes, CLOCK_MONOTONIC);
        if (!status) status = pthread_cond_init(&wakeup, &attributes);
        pthread_condattr_destroy(&attributes);
        if (status) return status;
        status = pthread_create(&controller, NULL, run, NULL);
        if (status) { pthread_cond_destroy(&wakeup); return status; }
        threaded = 1;
    }
    atomic_store_explicit(&accepting, 1, memory_order_relaxed);
    started = 1;
    return 0;
}

int mars_capture_launch(void) {
    if (!atomic_load_explicit(&accepting, memory_order_relaxed)) return 0;
    if (!atomic_load_explicit(&first_launch, memory_order_relaxed)) {
        uint_fast64_t expected = 0;
        atomic_compare_exchange_strong_explicit(&first_launch, &expected,
            now_ns(), memory_order_relaxed, memory_order_relaxed);
    }
    int capture = atomic_load_explicit(&enabled, memory_order_relaxed);
    atomic_fetch_add_explicit(capture ? &captured : &skipped, 1, memory_order_relaxed);
    return capture;
}

int mars_capture_stop(void) {
    atomic_store_explicit(&accepting, 0, memory_order_relaxed);
    pthread_mutex_lock(&state_lock);
    stopping = 1;
    if (threaded) pthread_cond_signal(&wakeup);
    pthread_mutex_unlock(&state_lock);
    if (threaded) {
        pthread_join(controller, NULL);
        threaded = 0;
    }
    pthread_mutex_lock(&state_lock);
    if (!stopped_at) {
        stopped_at = now_ns();
        account_active(stopped_at, atomic_load_explicit(&first_launch, memory_order_relaxed));
        atomic_store_explicit(&enabled, 0, memory_order_relaxed);
    }
    int status = failure;
    pthread_mutex_unlock(&state_lock);
    return status;
}

int mars_capture_stats(MarsGpuCaptureStats *stats) {
    if (!stats) return -1;
    pthread_mutex_lock(&state_lock);
    uint64_t epoch = atomic_load_explicit(&first_launch, memory_order_relaxed);
    uint64_t now = stopped_at ? stopped_at : now_ns();
    uint64_t active = atomic_load_explicit(&enabled, memory_order_relaxed);
    uint64_t since = active_since ? active_since : epoch;
    *stats = (MarsGpuCaptureStats) {
        .elapsed_ns = epoch && now >= epoch ? now - epoch : 0,
        .active_ns = accumulated_ns + (epoch && active && now >= since ? now - since : 0),
        .windows = epoch ? windows + 1 : 0,
        .captured_launches = atomic_load_explicit(&captured, memory_order_relaxed),
        .skipped_launches = atomic_load_explicit(&skipped, memory_order_relaxed),
        .active = active,
        .control_failures = failure != 0,
    };
    pthread_mutex_unlock(&state_lock);
    return 0;
}
