// Model: builds a list at each capacity 0..3, pushes a number of elements that straddles
// that capacity in both directions, then reads the length back through the consuming
// `neon_list_len`.
//
// THE INVARIANT: a list built with capacity `c` is empty with `cap` exactly `c`,
// `len <= cap` holds across pushes that straddle `c`, and the consuming `neon_list_len`
// reports the exact count and releases the reference it consumed.
//
// `neon_list_new_with_capacity` is the one constructor that hands `push` a buffer it did
// not allocate itself, and `cap` is the only record that the buffer is `c * w->size`
// bytes wide. Set `cap` to `c` while allocating for something else -- or allocate for `c`
// elements while telling `push` there is room for more -- and the first push past the
// real end writes off the buffer, with no crash at the site that caused it. The straddle
// is what makes this checkable: pushing fewer than `c`, exactly `c`, and more than `c`
// elements exercises the buffer under-filled, exactly full, and reallocated, and the
// `c == 0` arm additionally checks that no allocation happens at all and `data` stays
// NULL -- the state `push` then has to grow from scratch.
//
// `neon_list_len` is two lines, but the second is `neon_release`, so calling it is an
// ownership event: the caller hands over its reference. Codegen emits `len` on a value it
// may still be using, so it retains first; getting the balance wrong here leaks a whole
// list per call or frees one still in use. The `rc == 1` check after the retain-then-call
// below is what pins it. None of this is observable with a scalar element whose witness
// has a NULL `release` (rule 7), so the element is a 16-byte struct with a real
// retain/release and a per-identity counter, and the 16-byte width -- with a `tag`
// derived from the `id` -- turns a slot-width bug into a payload at the wrong offset
// rather than a coincidentally-correct read. If `cap` and the allocation ever disagreed
// about the width, the reads after the pushes would see a torn element.
//
// Verifies `src/list.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Baseline: 876 properties, VERIFICATION SUCCESSFUL. Two mutations, each reverted.
//
// 1. `malloc(cap * w->size)` -> `malloc(cap)`, the units mistake: capacity counted in
//    elements, allocated in bytes. Failed 18 of 869. The interesting part is where: not on
//    a claim about capacity, but downstream on pointer-arithmetic bounds in
//    `neon_list_push`, `neon_list_at` and `neon_list_drop`, and in the witness on "release
//    is handed a well-formed element" and "pointer outside object bounds in e->id". The
//    allocation looks fine until something uses it, and the model drives it far enough for
//    that. With the 16-byte element this is a 16x under-allocation -- shipped, an
//    immediate heap overflow on any list built through `with_capacity`.
//
// 2. `l->cap = (size_t)cap` dropped while the buffer is still allocated, so the list owns
//    space it does not know it has. Failed 1 of 869, precisely on "its capacity is exactly
//    what was requested". A single property and exactly the right one -- the buffer is
//    valid, so nothing downstream can see the mistake; the whole cost is that the first
//    push reallocs and `with_capacity` silently does nothing, which is a performance bug
//    no assertion elsewhere in the tree is positioned to catch.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. The capacity is enumerated concretely, 0..3, because `new_with_capacity` sizes its
//    `malloc` by `cap * w->size` and a symbolic allocation size exhausts the solver
//    rather than being merely slow (shape requirement 2, which applies to sized
//    allocation as well as to `memcpy`). The enumeration is exhaustive within the bound,
//    but coverage is BY ENUMERATION: capacities above 3 are NOT proved, and raising the
//    bound means adding arms.
//
// 2. A NEGATIVE CAPACITY IS NOT DRIVEN. `neon_list_new_with_capacity` takes an `int64_t`
//    and only allocates `if (cap > 0)`, so a negative argument silently yields an empty
//    list rather than trapping. NOT proved: that this is the intended behaviour -- it is
//    asserted nowhere in the runtime, and codegen is what currently prevents it.
//
// 3. No capacity here is large enough for `cap * w->size` to approach `size_t` overflow,
//    so the overflow check on that multiplication says nothing about huge lists; one
//    cannot be built inside a model.
//
// 4. The list is sole-owned throughout, so `ensure_unique` returns immediately and the
//    copy-on-write path inside `push` is never taken -- that is
//    `list-mutating-a-shared-list-copies-it`'s claim.
//
// 5. `neon_list_len`'s release is checked for one balanced use, not for the case where it
//    takes the count to zero and runs the drop. That path is reached at the end of this
//    model by `neon_release`, but not through `len` itself.

#include "../support/cbmc_support.h"

#include <stdio.h>

#include "libneon_rt.h"

// Rule 4. `neon_trap` calls `fflush`/`fprintf`, and CBMC's models of those pull a `FILE`
// and its buffers in at *every* trap site -- reachable here from `new_with_capacity`'s
// own out-of-memory check, from `push`'s, and from every OOM branch `--malloc-fail-null`
// opens. Left alone they account for most of the program's addressed objects and put it
// over CBMC's default `--object-bits 8`, which the shared CMake target does not override.
// What a trap prints is not a property of the list.
int fprintf(FILE* stream, const char* fmt, ...) { (void)stream; (void)fmt; return 0; }
int fflush(FILE* stream) { (void)stream; return 0; }

// ---- the element type and its witness ----

// 16 bytes, deliberately not a machine word: a slot-width bug then shows up as a payload
// landing at the wrong offset rather than as a coincidentally-correct read. `tag` is
// derived from `id`, so a torn or half-copied element is detectable and not just a
// swapped one.
typedef struct {
    uint64_t id;
    uint64_t tag;
} elem;

#define ELEM_TAG(i) (0xA51A51A500000000ULL ^ (uint64_t)(i))

#define NIDS 8
#define CAP_MAX 3
#define PUSH_MAX 3
static int live[NIDS]; // net owned references per identity

static void elem_retain(void* p) {
    elem* e = (elem*)p;
    PROVE(e->id < NIDS, "retain is handed a well-formed element");
    PROVE(e->tag == ELEM_TAG(e->id), "retain is handed intact element bytes");
    live[e->id]++;
}

static void elem_release(void* p) {
    elem* e = (elem*)p;
    PROVE(e->id < NIDS, "release is handed a well-formed element");
    PROVE(e->tag == ELEM_TAG(e->id), "release is handed intact element bytes");
    // The over-release oracle: it fails at the call in list.c that released once too
    // often, not at some later use.
    PROVE(live[e->id] > 0, "no element is released more times than it was retained");
    live[e->id]--;
}

static bool elem_eq(const void* a, const void* b) {
    return ((const elem*)a)->id == ((const elem*)b)->id;
}

static int elem_cmp(const void* a, const void* b) {
    uint64_t x = ((const elem*)a)->id, y = ((const elem*)b)->id;
    return x < y ? -1 : (x > y ? 1 : 0);
}

static const neon_witness ELEM_W = {
    .size = sizeof(elem),
    .retain = elem_retain,
    .release = elem_release,
    .eq = elem_eq,
    .cmp = elem_cmp,
};

// One staging slot for every element handed to the list, rather than a local in each
// caller (shape requirement 3): CBMC counts addressed objects, and a local per inlined
// push site is enough on its own to exceed the default `--object-bits`.
static elem staging;

static neon_list* push_owned(neon_list* l, uint64_t id) {
    staging.id = id;
    staging.tag = ELEM_TAG(id);
    live[id]++;
    return neon_list_push(l, &staging);
}

// `c` arrives concrete from the enumeration in `main`; see SCOPE note 1.
static void with_capacity(unsigned c) {
    neon_list* l = neon_list_new_with_capacity(&ELEM_W, (int64_t)c);

    PROVE(l->len == 0, "a list built with a capacity is still empty");
    PROVE(l->cap == c, "its capacity is exactly what was requested");
    PROVE(l->len <= l->cap, "len <= cap holds before any push");
    PROVE(l->header.rc == 1, "and it is sole-owned, so the first push is in place");
    if (c == 0) {
        PROVE(l->data == NULL, "a capacity of zero allocates no buffer at all");
    } else {
        PROVE(l->data != NULL, "a positive capacity allocates a buffer");
    }

    // The push count is symbolic and straddles `c` in both directions -- fewer than the
    // capacity, exactly it, and past it into the reallocating growth step.
    unsigned n = NONDET_UPTO(PUSH_MAX,
        "pushes into the preallocated buffer; the concrete enumeration of c straddles "
        "this in both directions, which is the point of this model. Growth from cap 0 in "
        "isolation is list-push-grows-without-losing-bytes's job");
    for (unsigned i = 0; i < PUSH_MAX; i++) { // constant bound, rule 3
        if (i >= n) break;
        l = push_owned(l, i);
        PROVE(l->len == i + 1, "each push into a preallocated buffer adds exactly one");
        PROVE(l->len <= l->cap, "len <= cap holds with a preallocated buffer too");
        PROVE(l->cap >= c, "a push never shrinks the capacity it was given");
    }

    // If the buffer and `cap` ever disagreed about the element width, these reads --
    // some of them past the original capacity, some inside it -- would see a torn element.
    for (unsigned i = 0; i < PUSH_MAX; i++) { // constant bound, rule 3
        if (i >= n) break;
        elem* s = (elem*)neon_list_at(l, (int64_t)i);
        PROVE(s == (elem*)(l->data + (size_t)i * ELEM_W.size),
              "at(i) addresses data + i * w->size in a preallocated buffer");
        PROVE(s->id == i && s->tag == ELEM_TAG(i),
              "element i is intact in slot i whether it fitted the capacity or not");
        PROVE(live[i] == 1, "the list owns exactly one reference per element");
    }

    // `neon_list_len` consumes its argument, so retain first to keep the list -- exactly
    // what codegen emits when the value is still live afterwards.
    neon_retain((neon_header*)l);
    int64_t got = neon_list_len(l);
    PROVE(got == (int64_t)n, "len() reports the exact number of elements");
    PROVE(l->header.rc == 1, "len() releases the reference it consumed, and only that one");
    PROVE(l->len == n, "and leaves the list itself alone");

    neon_release((neon_header*)l);
    for (unsigned k = 0; k < NIDS; k++) { // constant bound, rule 3
        PROVE(live[k] == 0, "dropping the list releases every element exactly once");
    }
}

int main(void) {
    // Exhaustive enumeration of the requested capacity, concretely so `malloc`'s size is
    // a constant; see SCOPE note 1.
    unsigned c = NONDET_UPTO(CAP_MAX,
        "requested capacity; harness dispatch only, it constrains no input the runtime "
        "sees beyond selecting one of the four concrete arms below");
    switch (c) {
        case 0: with_capacity(0); break;  // no buffer at all
        case 1: with_capacity(1); break;  // a buffer the pushes overrun
        case 2: with_capacity(2); break;  // a buffer the pushes can exactly fill
        default: with_capacity(3); break; // a buffer the pushes never overrun
    }
    return 0;
}
