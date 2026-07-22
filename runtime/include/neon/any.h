#ifndef NEON_ANY_H
#define NEON_ANY_H

// `any`: the one erasure boundary.
//
// A boxed value: the object header, the payload's value-witness (its size and how to
// release it), a type tag identifying the concrete type for `is`/`as`, and then the
// payload bytes. `neon_value` is a pointer to one of these.

#include <stdint.h>

#include "neon/core.h"

typedef struct neon_box {
    neon_header header;
    const neon_witness* w;
    uint64_t type_tag;
} neon_box;

neon_value neon_box_new(const void* payload, const neon_witness* w, uint64_t tag);

// The payload, but only if the box's tag matches `tag`; traps otherwise. This is the
// runtime half of `as`-from-`any`: an unguarded cast out of `any` asserts the box holds
// the target type, and the assertion is discharged here rather than trusted — reading
// the payload at the wrong type reinterprets bytes, which is how a structural
// `{ code: 99 }` boxed into `any` once forged an opaque record.
void* neon_box_expect(neon_value v, uint64_t tag);

static inline uint64_t neon_box_tag(neon_value v) {
    return ((neon_box*)v)->type_tag;
}
static inline void* neon_box_payload(neon_value v) {
    return (void*)((neon_box*)v + 1);
}

#endif
