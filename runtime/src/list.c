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
neon_list* neon_list_ensure_unique(neon_list* l) {
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

// `neon_list_set` for an element type the *caller* knows is not refcounted, with `sz` a
// constant at the call site.
//
// Both facts are static at every call: codegen knows the element repr exactly, so it knows
// whether the slot being overwritten needs releasing and how wide it is. The generic
// version cannot use either -- it reads `w->size` from the witness, which defeats
// specialising the copy, and tests `w->release`, which for a scalar is a load and a branch
// that can never fire. Passing the size as a literal lets the compiler fold the `memcpy`
// into a single store once this is inlined.
//
// Measured on the brainfuck benchmark (10^8 writes into a `List[i64]`): 0.84s -> 0.71s.
// The bounds check stays -- it costs nothing, because the caller's own check is adjacent
// after inlining and the compiler folds the pair (measured: removing it is not a win).
//
// PRECONDITION, and it is codegen's to keep: the element type must not be refcounted.
// Calling this for one that is leaks the value being overwritten.
// `neon_list_set_scalar` for a list the caller has ALREADY established is sole-owned, so
// there is no `rc` test and no possibility of a clone -- the list pointer is unchanged, and
// the caller may keep everything it knew about it.
//
// That last part is the point. The generic write returns a list that *might* differ, so a C
// compiler must discard `data`, `len` and every bounds fact across each call; on the
// brainfuck interpreter loop that cost 14.7% in reloading `data` alone, plus bounds checks
// that could not hoist. Returning nothing is what lets those stay in registers.
//
// PRECONDITION, and it is the optimiser's to keep: `l` must be sole-owned at this point,
// which `ir::unique` establishes by calling `neon_list_ensure_unique` once before the loop
// this is called from. Violating it mutates a list somebody else is holding -- silently,
// with no output difference until the other holder is read.
void neon_list_set_scalar_inplace(neon_list* l, int64_t i, const void* elem, size_t sz) {
    if (i < 0 || (size_t)i >= l->len) {
        neon_trap("list index out of range");
    }
    memcpy(l->data + (size_t)i * sz, elem, sz);
}

// Establish sole ownership of the LIST ELEMENT in slot `i` of `l`, and hand it back
// borrowed -- the slot keeps its reference, and the caller must not release what it got.
//
// PRECONDITIONS, both the optimiser's to keep, exactly as for `set_scalar_inplace`:
// `l` itself is sole-owned (so its slot may be overwritten), and its elements are
// `neon_list*` -- this is the per-level step of `ir::unique`'s nested-write rewrite,
// which only fires on `List[List[...]]` reprs. The bounds check traps with the same
// message as a read, because in the rewritten program this call stands where an
// element read stood.
//
// The body is `ensure_unique` aimed at a slot: the slot's reference is consumed by the
// clone-or-keep and the resulting sole reference is stored back, so ownership never
// changes hands and a shared element is cloned exactly once -- the next write through
// this slot sees `rc == 1` and pays a pointer test.
neon_list* neon_list_ensure_unique_at(neon_list* l, int64_t i) {
    if (i < 0 || (size_t)i >= l->len) {
        neon_trap("list index out of range");
    }
    neon_list** slot = (neon_list**)(l->data + (size_t)i * sizeof(neon_list*));
    *slot = neon_list_ensure_unique(*slot);
    return *slot;
}

neon_list* neon_list_set_scalar(neon_list* l, int64_t i, const void* elem, size_t sz) {
    if (i < 0 || (size_t)i >= l->len) {
        neon_trap("list index out of range");
    }
    l = neon_list_ensure_unique(l);
    memcpy(l->data + (size_t)i * sz, elem, sz);
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
