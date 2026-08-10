#ifndef NEON_ARENA_H
#define NEON_ARENA_H

// A per-fiber heap. The whole of `docs/design/fibers.md`'s memory model lives here: bump a
// pointer for fresh allocation, reclaim freed slots into size-class free lists (so memory
// PLATEAUS at the fiber's peak working set instead of growing with total allocation), and
// drop the entire arena in one operation at the fiber's death. Because Neon values are
// immutable there are no cycles, so this refcounting is complete — there is no tracing
// backstop and no compaction, and pointers are stable.
//
// The arena is explicitly passed here; in the running system `neon_alloc`/`neon_free` route
// to the CURRENT fiber's arena implicitly (an initial-exec `_Thread_local`), which isolation
// makes sound — a fiber only ever frees its own objects, and its objects are in its own
// arena. A dying fiber's arena is WALKED before it is dropped (`neon_arena_walk`, and
// `neon_fiber_free`'s two-pass teardown), so cleanups run and outgoing references are
// released; `neon_arena_drop` is the bulk-free that follows.

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

// Visit every LIVE object in the arena, in address order per chunk then the big list, calling
// `visit(header, ctx)` on each. This is the walkability the teardown of a dying fiber needs
// (docs/design/fibers.md): before the bulk-free, walk the live objects releasing their
// OUTGOING references (resource cleanups, shared-value decrements) — `visit` is that release.
//
// The visitor must not allocate in, or free from, this arena: it releases references that
// point OUTSIDE the arena, and leaves the arena's own objects for the bulk-free. Freed slots
// are skipped; a freed slot keeps its class so the walk can step past it.
void neon_arena_walk(const neon_arena* a, void (*visit)(neon_header*, void*), void* ctx);

// Drop the whole arena: every chunk and every big allocation, in one pass, then the control
// struct. Bulk-free only — pair it with neon_arena_walk first to release outgoing references.
void neon_arena_drop(neon_arena* a);

// Bytes the arena currently holds from the OS (chunks + big allocations). For tests and
// stats: this is the number that must PLATEAU under alloc/free churn.
size_t neon_arena_footprint(const neon_arena* a);

#endif
