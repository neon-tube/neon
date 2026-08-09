#ifndef NEON_ARENA_H
#define NEON_ARENA_H

// A per-fiber heap. The whole of `docs/design/fibers.md`'s memory model lives here: bump a
// pointer for fresh allocation, reclaim freed slots into size-class free lists (so memory
// PLATEAUS at the fiber's peak working set instead of growing with total allocation), and
// drop the entire arena in one operation at the fiber's death. Because Neon values are
// immutable there are no cycles, so this refcounting is complete — there is no tracing
// backstop and no compaction, and pointers are stable.
//
// This is slice 1: a standalone, explicitly-passed arena. In the fiber runtime the current
// fiber's arena is register-pinned and `neon_alloc`/`neon_free` route to it implicitly
// (isolation guarantees a fiber only ever frees its own objects, which are in its own
// arena). The teardown *walk* that releases a dying arena's OUTGOING references (Resource
// cleanups, shared-value decrements) before the bulk-free is slice 4; `neon_arena_drop`
// here bulk-frees the memory, which is exactly right for objects with no outgoing
// references (the case slice 1 tests).

#include <stddef.h>
#include <stdint.h>

#include "neon/core.h"

typedef struct neon_arena neon_arena;

// A fresh, empty arena. Its first chunk is allocated lazily on the first allocation, so an
// idle fiber's arena is just the small control struct.
neon_arena* neon_arena_create(void);

// Allocate `bytes` of payload after a `neon_header` (rc 1, the given drop), from `a`. The
// bump path is a pointer add; a reclaimed slot of the right class is reused first.
void* neon_arena_alloc(neon_arena* a, size_t bytes, void (*drop)(void*));

// Return one object to its arena's free list (or free it, if it was a big allocation). The
// object must have come from `a`.
void neon_arena_free(neon_arena* a, void* p);

// Drop the whole arena: every chunk and every big allocation, in one pass, then the control
// struct. Bulk-free only — the outgoing-reference walk is layered on in slice 4.
void neon_arena_drop(neon_arena* a);

// Bytes the arena currently holds from the OS (chunks + big allocations). For tests and
// stats: this is the number that must PLATEAU under alloc/free churn.
size_t neon_arena_footprint(const neon_arena* a);

#endif
