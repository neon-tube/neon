// Model: `neon_list_set_scalar_inplace` on a sole-owned list -- where the bytes land, and
// what the call is forbidden to disturb.
//
// THE INVARIANT: on a list with `rc == 1`, `neon_list_set_scalar_inplace(l, i, e, sz)`
// replaces exactly the `sz` bytes at `l->data + i * sz` -- the list pointer, `data`, `len`
// and `cap` are all the same afterwards, every other slot still holds its own bytes, and an
// index that is negative or `>= len` traps instead of writing.
//
// The invariance half is not a nicety, it is the entire reason the function exists.
// `neon_list_set` and `neon_list_set_scalar` both *return* a list that may differ from the
// one passed in, so a C compiler must discard `data`, `len` and every derived bounds fact
// across each call; on the brainfuck interpreter loop that cost 14.7% in reloading `data`
// alone. `set_scalar_inplace` returns void precisely so the caller may keep all of it live
// in registers across the write. If the function ever grew a path that reallocated, moved
// the buffer, or changed `len`, every caller `ir::unique` has rewritten would be reading
// stale state -- and nothing else in the runtime would notice, because the callers' cached
// values are invisible to it. That makes "nothing else changed" a property worth asserting
// on the runtime side rather than an incidental fact about three lines of C.
//
// The offset half is the other classic: the width comes from the *caller* as a literal, not
// from `l->w->size`, so a wrong literal at a call site writes across a slot boundary. The
// element here is 16 bytes with a tag derived from its identity, so a half-slot or
// off-by-one-slot write is visible as a torn or displaced element rather than as a
// coincidentally correct machine word.
//
// Verifies `src/list.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Three mutations of `neon_list_set_scalar_inplace`, each confirmed to fail and reverted.
// Baseline is 848 properties.
//
// `if (i < 0 || (size_t)i >= l->len)` weakened to `> l->len` -- the off-by-one that lets a
// write land one slot past the end. Failed on "an out-of-range or negative index traps
// rather than writing past the end of the list" (1 of 848). This is the mutation with the
// nastiest shipping cost: `ir::unique` rewrites writes inside a loop whose induction
// variable often runs to `len`, so the one bad index is the one the loop reaches every
// iteration, and it lands in the allocator's slack rather than in unmapped memory.
//
// The bounds check deleted outright. Failed on the same claim, plus nine memory-safety
// properties inside `neon_list_set_scalar_inplace` itself -- pointer arithmetic outside
// object bounds, the `(size_t)i` conversion overflow, and "memcpy destination region
// writeable" (10 of 841). Worth noting the shape: the trap claim alone would have caught
// it, but CBMC's own checks name the write site, which is what makes the trace readable.
//
// `memcpy(l->data + (size_t)i * sz, ...)` changed to `l->data + (size_t)i` -- the missing
// stride, the mistake of reading `i` as a byte offset because the generic `neon_list_set`
// looks similar. Failed on four claims (4 of 848): "slot i holds the element that was
// written", "slot i holds all sz bytes of it: the element is not torn across a slot
// boundary", "every other slot still holds its own element", and "and all of its bytes: the
// write did not spill into a neighbouring slot". The 16-byte tagged element is what makes
// this visible; an `int64_t` element would have made the mutation a no-op at `i` and only
// caught it at `i > 0`.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. THE ELEMENT TYPE IS A SCALAR, AND THAT IS THE CORRECT DOMAIN -- a deliberate,
//    reasoned departure from rule 7, not an oversight, and it must not be "fixed".
//    `neon_list_set_scalar_inplace` carries the documented precondition (list.c:103-104)
//    that the element type is NOT refcounted: it overwrites the slot with no release, so
//    calling it for a counted element leaks the value being overwritten. A refcounted
//    element is therefore outside the function's contract, and a model built on one would
//    be asserting properties of a call the runtime forbids. The witness here accordingly
//    has `retain` and `release` NULL, exactly as codegen emits for an `i64` or a `bool`.
//
//    Rule 7's actual demand -- exercise the case that makes the bug visible -- is met by
//    the *shape* of the element instead of by its ownership: 16 bytes, deliberately not a
//    machine word, with `tag` derived from `id`. The bug class this function can have is a
//    slot-width or slot-offset error, and a 16-byte self-checking element makes one visible
//    as a wrong or torn payload. An `int64_t` element would hide exactly that.
//
//    Consequently NOT proved anywhere in this set: that `set_scalar_inplace` behaves for a
//    refcounted element. It does not, by construction, and codegen must never emit it for
//    one.
//
// 2. THE PRECONDITION ITSELF IS NOT CHECKED HERE. This model enters with `rc == 1`. What
//    happens when the precondition is violated is the separate, deliberately
//    negatively-stated model `list-in-place-write-on-a-shared-list-corrupts-it`. Nothing
//    here proves that `ir::unique` actually establishes sole ownership before the loop it
//    rewrites -- that is a property of the Rust compiler pass, and no runtime model can
//    reach it.
//
// 3. LENGTH IS CONCRETELY 3. `neon_list_ensure_unique` is unreachable on this path (rc is
//    1, and `set_scalar_inplace` never calls it anyway), so the symbolic-`memcpy` hazard of
//    rule 4 does not bite -- but the list is still built at a literal length, because the
//    only thing a longer list adds is more slots of the same "other slot untouched" check.
//    Three is the least length with a written slot, a slot before it and a slot after it.
//    Not covered, therefore: any interaction between the write and buffer growth, which
//    `set_scalar_inplace` cannot cause since it never reallocates.
//
// 4. `sz` is passed as `sizeof(elem)`, agreeing with the witness. The model cannot catch a
//    call site that passes the wrong literal, because it is itself the call site; it proves
//    that a *correct* literal addresses the slot the witness would have. Catching a wrong
//    literal needs a check in codegen's emission, where the literal is chosen.
//
// 5. Out-of-memory is not a recoverable path in this runtime -- every allocation failure
//    reaches `neon_trap`, which `_exit`s. CBMC does take those branches under
//    `--malloc-fail-null` and proves nothing is dereferenced before the trap, but a leak
//    check cannot fire past a trap, so "no leak on OOM" is vacuous by design rather than
//    proved.

#include "../support/cbmc_support.h"

#include <stdio.h>

#include "libneon_rt.h"

// Rule 4. `neon_trap` calls `fflush`/`fprintf`, and this model reaches a trap from every
// allocation check as well as from the out-of-range index below. CBMC's models of those
// pull a `FILE` and its buffers into each of those sites, which alone accounts for most of
// the program's addressed objects under the default `--object-bits 8`. What a trap prints
// is not a property of the list, and the trap still terminates the path via `_exit`.
int fprintf(FILE* stream, const char* fmt, ...) { (void)stream; (void)fmt; return 0; }
int fflush(FILE* stream) { (void)stream; return 0; }

// Not in the support header's nondet family, and indexing is the one place this model needs
// a *signed* 64-bit unconstrained value: a negative index must trap.
int64_t nondet_int64(void);

// ---- the element type and its witness ----
//
// 16 bytes, deliberately not a machine word, with `tag` derived from `id` so that a torn or
// half-copied element is detectable and not merely a swapped one. `retain`/`release` are
// NULL: this is a scalar element, which is the documented domain of the function under
// test. See SCOPE note 1 -- that choice is load-bearing and reasoned, not an omission.
typedef struct {
    uint64_t id;
    uint64_t tag;
} elem;

#define ELEM_TAG(i) (0xA51A51A500000000ULL ^ (uint64_t)(i))
#define ELEM_SZ sizeof(elem)

// `eq` and `cmp` are NULL too: nothing on this path compares elements, and every
// address-taken function of a witness-callback type is another indirect-call target CBMC
// must branch over.
static const neon_witness ELEM_W = {
    .size = sizeof(elem),
    .retain = NULL,
    .release = NULL,
    .eq = NULL,
    .cmp = NULL,
};

// One staging slot for every element handed to the list, rather than a local per call site.
// CBMC counts *addressed* objects; the list copies bytes out of whatever address it is
// given, so a single reused slot exercises identical code.
static elem staging;

// The list holds three elements, identities 0..2. Concrete; see SCOPE note 3.
#define N 3
// The identity written by the in-place store, distinct from everything already present so
// that overwriting slot i is distinguishable from writing it back unchanged.
#define FRESH 7

static neon_list* build(void) {
    neon_list* l = neon_list_new(&ELEM_W);
    for (unsigned i = 0; i < N; i++) { // constant bound, rule 3
        staging.id = i;
        staging.tag = ELEM_TAG(i);
        l = neon_list_push(l, &staging);
    }
    return l;
}

int main(void) {
    neon_list* l = build();
    PROVE(l->len == N, "the fixture holds exactly three elements");
    PROVE(l->header.rc == 1,
          "the fixture is sole-owned, which is this function's precondition");

    // Everything the caller is entitled to keep live across the write.
    neon_list* old_l = l;
    char* old_data = l->data;
    size_t old_len = l->len;
    size_t old_cap = l->cap;
    const neon_witness* old_w = l->w;

    unsigned i = NONDET_UPTO(N - 1, "the slot written; every in-range index of the "
                                    "fixture is explored, so the model covers the first "
                                    "slot, the last, and one with a neighbour on each side");

    staging.id = FRESH;
    staging.tag = ELEM_TAG(FRESH);
    neon_list_set_scalar_inplace(l, (int64_t)i, &staging, ELEM_SZ);

    // ---- nothing moved ----
    //
    // The function returns void, so `l` is trivially the same pointer; these assert the
    // stronger claim that the object it points at is unchanged in every field the caller
    // caches. This is the performance argument for the function, stated as a property.
    PROVE(l == old_l, "the list pointer is unchanged: no copy was taken");
    PROVE(l->data == old_data,
          "the buffer is neither moved nor reallocated, so a caller's cached `data` "
          "stays valid across the write");
    PROVE(l->len == old_len, "an in-place write does not change the length");
    PROVE(l->cap == old_cap, "an in-place write does not change the capacity");
    PROVE(l->w == old_w, "an in-place write does not change the element witness");
    PROVE(l->header.rc == 1, "an in-place write neither retains nor releases the list");

    // ---- the bytes landed in slot i, whole ----
    elem* s = (elem*)(l->data + (size_t)i * ELEM_SZ);
    PROVE(s == (elem*)neon_list_at(l, (int64_t)i),
          "the in-place write addresses data + i * sz, the same slot the witness-driven "
          "accessor addresses");
    PROVE(s->id == FRESH, "slot i holds the element that was written");
    PROVE(s->tag == ELEM_TAG(FRESH),
          "slot i holds all sz bytes of it: the element is not torn across a slot boundary");

    // ---- and disturbed nothing else ----
    for (unsigned k = 0; k < N; k++) { // constant bound, rule 3
        if (k == i) continue;
        elem* o = (elem*)neon_list_at(l, (int64_t)k);
        PROVE(o->id == k, "every other slot still holds its own element");
        PROVE(o->tag == ELEM_TAG(k),
              "and all of its bytes: the write did not spill into a neighbouring slot");
    }

    // ---- an index outside the list traps ----
    //
    // Branched rather than sequenced, so the checks above still run on the other arm: a
    // trap ends in `_exit`, which the support header's stub makes a dead end.
    if (nondet_bool()) {
        int64_t bad = nondet_int64();
        ASSUME(bad < 0 || (uint64_t)bad >= (uint64_t)l->len,
               "splits the index space; this arm is the out-of-range half, otherwise "
               "unconstrained so negative, == len and huge are all reachable. The in-range "
               "half is not excluded from the model -- it is the arm checked above");
        staging.id = FRESH;
        staging.tag = ELEM_TAG(FRESH);
        neon_list_set_scalar_inplace(l, bad, &staging, ELEM_SZ);
        PROVE(0, "an out-of-range or negative index traps rather than writing past the "
                 "end of the list");
    }

    neon_release((neon_header*)l);
    return 0;
}
