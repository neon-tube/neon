#include "libneon_rt.h"

// ---- any (boxed erasure) ----

static void neon_box_drop(void* p) {
    neon_box* b = (neon_box*)p;
    if (b->w->release) {
        b->w->release((void*)(b + 1));
    }
    neon_free(b);
}

void* neon_box_expect(neon_value v, uint64_t tag) {
    neon_box* b = (neon_box*)v;
    if (b->type_tag != tag) {
        neon_trap("cast from any to a type the value does not hold");
    }
    return (void*)(b + 1);
}

neon_value neon_box_new(const void* payload, const neon_witness* w, uint64_t tag) {
    size_t extra = sizeof(neon_box) - sizeof(neon_header) + w->size;
    neon_box* b = (neon_box*)neon_alloc(extra, neon_box_drop);
    b->w = w;
    b->type_tag = tag;
    memcpy((void*)(b + 1), payload, w->size);
    return (neon_value)b;
}
