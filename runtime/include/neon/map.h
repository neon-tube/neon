#ifndef NEON_MAP_H
#define NEON_MAP_H

// An open-addressed hash map.

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "neon/core.h"
#include "neon/list.h" // neon_map_keys / neon_map_values return lists

// `ctrl` marks each slot empty/tombstone/full, and keys and values live in parallel arrays
// sized by their witnesses. The header is first, so a `neon_map*` is also its
// `neon_header*`.
#define NEON_MAP_EMPTY 0u
#define NEON_MAP_DEAD 1u
#define NEON_MAP_FULL 2u

typedef struct neon_map {
    neon_header header;
    const neon_key_witness* kw;
    const neon_witness* vw;
    size_t len;
    size_t cap;
    unsigned char* ctrl;
    char* keys;
    char* vals;
} neon_map;

neon_map* neon_map_new(const neon_key_witness* kw, const neon_witness* vw);
int64_t neon_map_len(neon_map* m);                                  // consumes m
// `contains` and `set` *consume* their key, like any other native: `set` moves it into the
// table, or drops it when the table already holds that key. `at` and `find` borrow it --
// they are reached through `Op::Index`, whose operands the refcount pass releases itself,
// so releasing here too would double-free.
bool neon_map_contains(neon_map* m, const void* key);               // consumes m and key
neon_map* neon_map_set(neon_map* m, const void* key, const void* val); // consumes m and key
void* neon_map_at(neon_map* m, const void* key);   // borrows both; traps if absent
void* neon_map_find(neon_map* m, const void* key); // borrows both; NULL when absent
bool neon_map_eq(neon_map* a, neon_map* b);        // borrows both; same keys, equal values
neon_map* neon_map_remove(neon_map* m, const void* key); // consumes m and key

// How `neon_map_update` calls back into the program. The runtime cannot call a `(V) -> V`
// closure itself: the C signature of that call depends on `V`, which only codegen knows. So
// codegen emits one of these per instantiation -- it reads `in` at the right width, calls
// the closure, and stores the owned result to `out`. Same division of labour as the
// `cleanup` shim `neon_resource_new` takes.
//
// It consumes the value at `in` (the closure does) and produces an owned value at `out`.
//
// `in` and `out` may alias -- `update` passes the same slot for both on the present path --
// so an implementation must finish reading `in` before it writes `out`. Loading `in` into a
// local first, which is what the emitted shim does, satisfies this by construction.
typedef void (*neon_map_updater)(neon_closure f, const void* in, void* out);

// Set `key` to `f` applied to its current value, or to `f(fallback)` when absent. Consumes
// the map, the key, the fallback and the closure.
//
// The point of this over `get_or` + `set` is that it hashes and probes *once*. Written out,
// the counting idiom `set(m, k, get_or(m, k, 0) + 1)` is three passes -- `get_or` is itself
// `contains` then an index -- and each one re-hashes the key.
neon_map* neon_map_update(neon_map* m, const void* key, const void* fallback, neon_closure f,
                          neon_map_updater call);
neon_list* neon_map_keys(neon_map* m, const neon_witness* w);   // consumes m
neon_list* neon_map_values(neon_map* m, const neon_witness* w); // consumes m


// The witness `copy` for a map slot: rebuilt entry by entry, keys and values deep-copied.
// Used by emitted witness tables.
void neon_wcopy_map(const void* src, void* dst);

#endif
