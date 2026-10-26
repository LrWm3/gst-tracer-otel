/* Test-only NVTX export table. Exercises the actual injection handshake and
 * callbacks without requiring an NVTX SDK on ordinary CI machines. */
#include <assert.h>
#include <stddef.h>
#include <stdint.h>
typedef void (*Function)(void);
typedef struct { size_t size; int (*module)(int, Function ***, unsigned *); } Callbacks;
static Function functions[16];
static Function *table[16];
static int module(int id, Function ***out, unsigned *count) {
    if (id != 5) return 0;
    *out = table; *count = 16; return 1;
}
static const void *get_table(uint32_t id) {
    static const Callbacks callbacks = {sizeof(Callbacks), module};
    return id == 1 ? &callbacks : NULL;
}
extern int InitializeInjectionNvtx2(const void *(*)(uint32_t));
void mars_nvtx_mock_initialize(void) {
    for (unsigned i = 0; i < 16; ++i) table[i] = &functions[i];
    assert(InitializeInjectionNvtx2(get_table) == 1);
    assert(InitializeInjectionNvtx2(NULL) == 0);
}
uintptr_t mars_nvtx_mock_create(const char *name) {
    return (uintptr_t)((void *(*)(const char *))functions[12])(name);
}
void mars_nvtx_mock_destroy(uintptr_t domain) { ((void (*)(void *))functions[14])((void *)domain); }
int mars_nvtx_mock_push(uintptr_t domain) { return ((int (*)(void *, const void *))functions[4])((void *)domain, NULL); }
int mars_nvtx_mock_pop(uintptr_t domain) { return ((int (*)(void *))functions[5])((void *)domain); }
