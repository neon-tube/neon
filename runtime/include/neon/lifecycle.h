#ifndef NEON_LIFECYCLE_H
#define NEON_LIFECYCLE_H

// Runtime startup and the refcount primitives every heap object goes through.

#include <stddef.h>

#include "neon/core.h"

void neon_rt_init(void);
void neon_retain(neon_header* h);
void neon_release(neon_header* h);
void* neon_alloc(size_t bytes, void (*drop)(void*));
void neon_free(void* p);

// A string's share of the refcount, taken and given back.
//
// Here rather than in `core.h` only because they need `neon_retain`/`neon_release`, which
// are declared just above. They are the counterpart of `neon_str_data`: the point of both
// is that a small-string optimisation changes these four functions and nothing else.
//
// Today a string is always a view into a counted allocation, and `owner == NULL` means a
// static literal, for which both of these are already no-ops. Under SSO an inline string
// has no owner either, and it will take the same path -- which is the property that makes
// these safe to write now and cheap to change later. Codegen emits calls to *these*, not
// `neon_retain(x.owner)`, so the emitted C needs no revisiting when the layout moves.
static inline void neon_str_retain(neon_str s) {
    neon_retain(s.owner);
}

static inline void neon_str_release(neon_str s) {
    neon_release(s.owner);
}

#endif
