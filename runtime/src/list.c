#include "libneon_rt.h"

#include <stdlib.h>

// ---- list ----

int64_t neon_list_len(neon_list* l) {
    int64_t n = (int64_t)l->len;
    neon_release((neon_header*)l);
    return n;
}

void* neon_list_at(neon_list* l, int64_t i) {
    if (i < 0 || (size_t)i >= l->len) {
        neon_trap("list index out of range");
    }
    return l->data + (size_t)i * l->w->size;
}

static void neon_list_drop(void* p) {
    neon_list* l = (neon_list*)p;
    if (l->w->release) {
        for (size_t i = 0; i < l->len; i++) {
            l->w->release(l->data + i * l->w->size);
        }
    }
    free(l->data);
    neon_free(l); // frees the header+body allocation
}

neon_list* neon_list_new(const neon_witness* w) {
    neon_list* l = (neon_list*)neon_alloc(sizeof(neon_list) - sizeof(neon_header), neon_list_drop);
    l->w = w;
    l->len = 0;
    l->cap = 0;
    l->data = NULL;
    return l;
}

neon_list* neon_list_new_with_capacity(const neon_witness* w, int64_t cap) {
    neon_list* l = neon_list_new(w);
    if (cap > 0) {
        l->cap = (size_t)cap;
        l->data = (char*)malloc((size_t)cap * w->size);
        if (l->data == NULL) neon_trap("out of memory");
    }
    return l;
}


// Copy a shared list before a mutation, retaining each element for the copy.
static neon_list* neon_list_ensure_unique(neon_list* l) {
    if (l->header.rc == 1) {
        return l;
    }
    size_t sz = l->w->size;
    neon_list* c = neon_list_new_with_capacity(l->w, (int64_t)(l->len ? l->len : 1));
    // An empty list has `data == NULL`, and `memcpy` requires valid pointers even for a
    // count of zero (C17 7.24.1p2). It also carries `nonnull`, from which a compiler may
    // infer the arguments are non-NULL and delete later checks -- so this is exploitable
    // UB, not a technicality. Found by the CBMC model; UBSan reports it too, but no corpus
    // program copies an empty list.
    if (l->len != 0) {
        memcpy(c->data, l->data, l->len * sz);
    }
    c->len = l->len;
    if (l->w->retain) {
        for (size_t i = 0; i < c->len; i++) l->w->retain(c->data + i * sz);
    }
    neon_release((neon_header*)l);
    return c;
}


neon_list* neon_list_push(neon_list* l, const void* elem) {
    size_t sz = l->w->size;
    l = neon_list_ensure_unique(l);
    if (l->len == l->cap) {
        size_t ncap = l->cap ? l->cap * 2 : 4;
        l->data = (char*)realloc(l->data, ncap * sz);
        if (l->data == NULL) neon_trap("out of memory");
        l->cap = ncap;
    }
    memcpy(l->data + l->len * sz, elem, sz);
    l->len++;
    return l;
}

neon_list* neon_list_set(neon_list* l, int64_t i, const void* elem) {
    if (i < 0 || (size_t)i >= l->len) {
        neon_trap("list index out of range");
    }
    size_t sz = l->w->size;
    l = neon_list_ensure_unique(l);
    char* slot = l->data + (size_t)i * sz;
    if (l->w->release) l->w->release(slot);
    memcpy(slot, elem, sz);
    return l;
}

neon_list* neon_list_concat(neon_list* a, neon_list* b) {
    size_t sz = a->w->size;
    neon_list* r = neon_list_new_with_capacity(a->w, (int64_t)(a->len + b->len));
    // Same as `ensure_unique`: an empty operand has `data == NULL`, and concatenating two
    // empty lists additionally forms `NULL + 0`, which is UB in its own right.
    if (a->len != 0) {
        memcpy(r->data, a->data, a->len * sz);
    }
    if (b->len != 0) {
        memcpy(r->data + a->len * sz, b->data, b->len * sz);
    }
    r->len = a->len + b->len;
    if (a->w->retain) {
        for (size_t i = 0; i < r->len; i++) a->w->retain(r->data + i * sz);
    }
    neon_release((neon_header*)a);
    neon_release((neon_header*)b);
    return r;
}

// Lexicographic over elements: the first differing element decides, and if one list is a
// prefix of the other the shorter sorts first. Both lists are borrowed -- comparison reads.
//
// The element compare comes from the witness, so this one function serves every element
// type, including nested lists: an inner list's elements are reached through *its* witness
// on the recursive call.
int neon_list_cmp(const neon_list* a, const neon_list* b) {
    size_t sz = a->w->size;
    size_t n = a->len < b->len ? a->len : b->len;
    for (size_t i = 0; i < n; i++) {
        int c = a->w->cmp(a->data + i * sz, b->data + i * sz);
        if (c != 0) {
            return c;
        }
    }
    return a->len < b->len ? -1 : (a->len > b->len ? 1 : 0);
}

// Equality could be `neon_list_cmp(a, b) == 0`, but a length check rejects most unequal
// pairs without touching an element, and it is the answer `==` asks for.
bool neon_list_eq(const neon_list* a, const neon_list* b) {
    if (a->len != b->len) {
        return false;
    }
    size_t sz = a->w->size;
    for (size_t i = 0; i < a->len; i++) {
        if (!a->w->eq(a->data + i * sz, b->data + i * sz)) {
            return false;
        }
    }
    return true;
}
