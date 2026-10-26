/* Optional NVTX injection, independent of CUDA/CUPTI and CPU sampling.
 * This small subset of the NVTX injection ABI is defined by NVIDIA's
 * c/include/nvtx3/nvtxDetail/nvtxTypes.h:
 * https://github.com/NVIDIA/NVTX/blob/v3.2.1/c/include/nvtx3/nvtxDetail/nvtxTypes.h
 * We never read event-attribute structs or call back into NVTX. Unsupported
 * APIs retain NVTX's normal no-tool behavior. Keep this DSO resident until exit.
 */
#include "nvtx_adapter.h"
#include <stddef.h>
#include <stdatomic.h>

typedef void (*Function)(void);
typedef struct {
    size_t size;
    int (*module)(int module, Function ***table, unsigned *count);
} Callbacks;
typedef const void *(*GetTable)(uint32_t);

static _Atomic(MarsNvtxReceive) receiver;

int mars_nvtx_start_v1(MarsNvtxReceive receive) {
    if (!receive) return -1;
    MarsNvtxReceive expected = NULL;
    return atomic_compare_exchange_strong(&receiver, &expected, receive) ? 0 : -1;
}

static uintptr_t send(uint32_t kind, uintptr_t domain, const char *name) {
    MarsNvtxReceive receive = atomic_load(&receiver);
    return receive ? receive(kind, domain, name) : 0;
}
static void *create(const char *name) { return (void *)send(1, 0, name); }
static void destroy(void *domain) { (void)send(2, (uintptr_t)domain, NULL); }
static int push(void *domain, const void *attributes) {
    (void)attributes;
    return (int)send(3, (uintptr_t)domain, NULL) - 1;
}
static int pop(void *domain) { return (int)send(4, (uintptr_t)domain, NULL) - 1; }

int InitializeInjectionNvtx2(GetTable get_table) {
    if (!get_table) return 0;
    const Callbacks *callbacks = get_table(1); /* NVTX_ETID_CALLBACKS */
    if (!callbacks || callbacks->size < sizeof(*callbacks) || !callbacks->module) return 0;
    Function **table = NULL;
    unsigned count = 0;
    /* NVTX_CB_MODULE_CORE2, with the stable domain API callback IDs. */
    if (!callbacks->module(5, &table, &count) || !table || count <= 14) return 0;
    const unsigned ids[] = {4, 5, 12, 14};
    const Function functions[] = {(Function)push, (Function)pop, (Function)create, (Function)destroy};
    for (unsigned i = 0; i < sizeof(ids) / sizeof(ids[0]); ++i)
        if (!table[ids[i]]) return 0;
    for (unsigned i = 0; i < sizeof(ids) / sizeof(ids[0]); ++i)
        *table[ids[i]] = functions[i];
    return 1;
}
