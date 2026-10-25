/* Loaded only by lifecycle_soak: exercise CPU sampling under the loader lock. */
#include <time.h>
__attribute__((noinline)) void mars_soak_module_work(void) {
  struct timespec start, now;
  clock_gettime(CLOCK_THREAD_CPUTIME_ID, &start);
  do {
    clock_gettime(CLOCK_THREAD_CPUTIME_ID, &now);
  } while ((now.tv_sec - start.tv_sec) * 1000000000L + now.tv_nsec - start.tv_nsec < 15000000L);
}
__attribute__((constructor)) static void loaded(void) { mars_soak_module_work(); }
__attribute__((destructor)) static void unloaded(void) { mars_soak_module_work(); }
