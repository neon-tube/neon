#ifndef NEON_LIST_H
#define NEON_LIST_H

// Lists: elements are moved in and out by codegen through the void* slot pointer.

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "neon/core.h"
// For `neon_list_at_scalar`, which traps in-header rather than calling out to do it.
#include "neon/trap.h"

// A list stores its elements inline in `data` (len used of cap slots, each `w->size`
// bytes). The header is first, so a `neon_list*` is also its `neon_header*`.
typedef struct neon_list {
    neon_header header;
    const neon_witness* w;
    size_t len;
    size_t cap;
    char* data;
} neon_list;

neon_list* neon_list_new(const neon_witness* w);
neon_list* neon_list_new_with_capacity(const neon_witness* w, int64_t cap);
int64_t neon_list_len(neon_list* l);                        // consumes l
void* neon_list_at(neon_list* l, int64_t i); // borrows l; slot pointer, traps OOB
neon_list* neon_list_push(neon_list* l, const void* elem);  // consumes l, moves *elem in
neon_list* neon_list_set(neon_list* l, int64_t i, const void* elem); // consumes l, traps OOB
// As `neon_list_set`, for an element the caller knows is NOT refcounted, with `sz`
// constant at the call site. Skips the witness entirely: no release of the overwritten
// slot, and the copy folds to a store. Calling it for a refcounted element leaks.
neon_list* neon_list_set_scalar(neon_list* l, int64_t i, const void* elem, size_t sz);
// Establish sole ownership: returns `l` when it is already the only reference, and a
// private copy otherwise (consuming the original). Called ONCE before a loop that then
// writes in place, rather than per write.
neon_list* neon_list_ensure_unique(neon_list* l);
// A write to a list the caller has already established is sole-owned. Returns nothing:
// the pointer cannot change, which is exactly what lets the caller keep `data` and its
// bounds facts live across it. See the precondition on the definition.
void neon_list_set_scalar_inplace(neon_list* l, int64_t i, const void* elem, size_t sz);
// Make the `neon_list*` ELEMENT in slot `i` of sole-owned `l` itself sole-owned, and
// return it borrowed: the slot keeps ownership. The per-level step of the nested
// in-place write; see the preconditions on the definition.
neon_list* neon_list_ensure_unique_at(neon_list* l, int64_t i);

// `neon_list_at` with the element width supplied by the caller instead of read from the
// witness.
//
// The generic version loads `l->w->size` and multiplies, and the witness is opaque to the
// C compiler, so the multiply survives even when the whole function is inlined: the hot
// loop of a `List[i64]` walk carried `mov (%rax),%rdi; imul %r12,%rdi` where it wanted a
// scale-8 addressing mode. Codegen always knows the element's C type -- it casts the
// result to it on the very next token -- so the size is a literal at every call and the
// multiply folds away.
//
// `static inline` rather than a runtime symbol, unlike its `set` counterpart: it is three
// lines and it is the single hottest thing a loop over a list does, so it should not
// depend on LTO reaching into the archive to be inlined.
//
// Unlike `neon_list_set_scalar` there is no precondition about refcounting: reading a slot
// touches the witness for nothing but its width, whatever the element type is.
static inline void* neon_list_at_scalar(neon_list* l, int64_t i, size_t sz) {
    if (i < 0 || (size_t)i >= l->len) {
        neon_trap("list index out of range");
    }
    return l->data + (size_t)i * sz;
}
neon_list* neon_list_concat(neon_list* a, neon_list* b);    // consumes both
int neon_list_cmp(const neon_list* a, const neon_list* b);  // borrows both; -1/0/1
bool neon_list_eq(const neon_list* a, const neon_list* b);  // borrows both

#endif
