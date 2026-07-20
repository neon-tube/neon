// Model: indexes a built list at a wholly unconstrained signed index, and separately at
// every in-range one.
//
// THE INVARIANT: `neon_list_at` traps for any index outside `0 .. len - 1`, and for an
// in-range `i` returns exactly `data + i * w->size`.
//
// Both halves are load-bearing and neither is obvious from the source. The guard is
// `i < 0 || (size_t)i >= l->len`, and the cast is what makes it correct: `i` arrives as a
// signed 64-bit value straight from a Neon expression, so without the `i < 0` arm a
// negative index would convert to an enormous `size_t` -- which happens to be caught here,
// but reverse the order of the two tests and it is a wild read below the buffer. This
// model leaves the index completely unconstrained apart from splitting it into the two
// halves, so -1, `len` exactly, and 2^63 - 1 are all reached.
//
// The address half is the same slot-width question the push model asks, seen from the
// read side: codegen hands elements to the list by address and reads `w->size` bytes back
// through the returned pointer, so `at` returning a slot computed with the wrong width is
// a memory-safety bug rather than a wrong answer. The predecessor project shipped exactly
// that -- a constructor emitting 24-byte slots that `at` addressed as 8, an ASan
// stack-buffer-overflow on every `list::new()`. A scalar element with a NULL `release`
// cannot see it, so the element here is a 16-byte struct whose `tag` is derived from its
// `id`, with a per-identity ownership counter (rule 7).
//
// The trap arm is asserted by an unreachable `PROVE(0)` after the call: the support
// header's `_exit` stub makes anything after a trap infeasible, so that line firing means
// `at` returned a slot instead of trapping.
//
// Verifies `src/list.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Baseline: 796 properties, VERIFICATION SUCCESSFUL. Three mutations, each reverted.
//
// 1. THE HISTORICAL BUG. `neon_list_at`'s slot arithmetic given a literal width:
//    `l->data + i * l->w->size` -> `l->data + i * 8`. The predecessor shipped exactly
//    this -- 24-byte slots addressed as 8 -- and it was an ASan overflow on every
//    `list::new()`. Failed 3 of 784, on "an in-range at(i) is exactly data + i *
//    w->size", "and therefore reads back the element pushed into slot i", and "with its
//    bytes intact, so the slot width is right". The third is the one that matters: the
//    16-byte element's `tag` is derived from its `id`, so a wrong stride is caught as a
//    torn read even where the misaddressed slot happens to be in bounds.
//
// 2. The `i < 0` half of the bounds check dropped, leaving `(size_t)i >= l->len`. Failed
//    1 of 796 -- but on CBMC's own "arithmetic overflow on signed to unsigned type
//    conversion in (size_t)i" at line 14, not on this model's trap assertion. Worth
//    knowing: the negative index is caught by the conversion check the shared CMake
//    args turn on, not by "an out-of-range or negative list index traps". The harness
//    would still have found it, but the property that fires is the toolchain's.
//
// 3. Off-by-one: `>=` weakened to `>`, letting an index of exactly `len` through. Failed
//    5 of 796 on "an out-of-range or negative list index traps rather than returning a
//    slot", plus pointer-arithmetic failures at line 17 and a leaked allocation -- the
//    trap that should have fired is what frees. Shipped, this is a one-slot read past
//    every list, the classic form of the bug.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. `neon_list_set` and `neon_list_set_scalar` repeat the same bounds test on their own
//    index. NOT proved here: that those copies of the guard are also correct. They are
//    separate lines of source and a divergence between them would not be caught.
//
// 2. Lengths reach 3, not the growth threshold. The branch `at` takes is decided by
//    `i < 0 || i >= len` alone, so no longer list reaches a different one, but it does
//    mean NOT proved: that `at` addresses correctly into a buffer that has been
//    `realloc`ed. That is `list-push-grows-without-losing-bytes`'s claim.
//
// 3. `w->size` is 16 for every list here. NOT proved: that the arithmetic is right for a
//    width that is not a power of two, or for the 8-byte scalar repr codegen also emits.
//
// ---- Assumptions ----
//
// Two, both bound-encoding, and one splitting the index space:
//
//   * `n >= 1` in the in-range half -- an empty list has no in-range index to check the
//     address of. The empty list's *trap* behaviour is not excluded: `n` reaches 0 in the
//     out-of-range half, where every index including 0 must trap.
//   * `i < 0 || (uint64_t)i >= len` in the out-of-range half, otherwise unconstrained.
//     This is a split of the index space, not a narrowing of it: the complementary half
//     is the in-range arm below, which reads every in-range index of every reachable
//     length.

#include "../support/cbmc_support.h"

#include <stdio.h>

#include "libneon_rt.h"

// Rule 4. `neon_trap` calls `fflush`/`fprintf`, and CBMC's models of those pull a `FILE`
// and its buffers in at *every* trap site -- and this model exists to reach a trap, on
// top of the ones every allocation check and `--malloc-fail-null` branch opens. Left
// alone they account for most of the program's addressed objects and put it over CBMC's
// default `--object-bits 8`, which the shared CMake target does not override. Nothing
// under test is lost: what a trap prints is not a property of the list, and the trap
// still terminates the path via `_exit`.
int fprintf(FILE* stream, const char* fmt, ...) { (void)stream; (void)fmt; return 0; }
int fflush(FILE* stream) { (void)stream; return 0; }

// Not in the support header's nondet family, and indexing is the one place a model needs
// a *signed* 64-bit unconstrained value: a negative index must trap.
int64_t nondet_int64(void);

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
#define SMALLN 3
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

// Build a fresh list holding identities `0 .. n - 1`.
static neon_list* build_run(unsigned n) {
    neon_list* l = neon_list_new(&ELEM_W);
    for (unsigned i = 0; i < SMALLN; i++) { // constant bound, rule 3
        if (i >= n) break;
        l = push_owned(l, i);
    }
    return l;
}

// The out-of-range half: an unconstrained signed index that is not in `0 .. len - 1`.
static void at_outside_traps(void) {
    unsigned n = NONDET_UPTO(SMALLN,
        "list length; the branch `at` takes is decided by `i < 0 || i >= len` alone, so "
        "no length beyond a couple reaches a different one. 0 is included, which is the "
        "case where every index whatsoever must trap");

    neon_list* l = build_run(n);

    int64_t i = nondet_int64();
    ASSUME(i < 0 || (uint64_t)i >= (uint64_t)l->len,
           "splits the index space; this is the out-of-range half, otherwise entirely "
           "unconstrained so negative, == len and huge are all reachable. The in-range "
           "half is the other arm of this model, not a hole");

    void* p = neon_list_at(l, i);
    (void)p;
    // Unreachable: `neon_list_at` traps and the support header's `_exit` stub makes
    // anything after a trap infeasible. If indexing ever returned a slot instead of
    // trapping, this fires.
    PROVE(0, "an out-of-range or negative list index traps rather than returning a slot");
}

// The in-range half: every valid index of every reachable length addresses its own slot.
static void at_inside_addresses_its_slot(void) {
    unsigned n = NONDET_UPTO(SMALLN, "list length; every in-range index of each is read");
    ASSUME(n >= 1, "an empty list has no in-range index; its trap behaviour is covered "
                   "by the other arm, where n reaches 0");

    neon_list* l = build_run(n);

    for (unsigned i = 0; i < SMALLN; i++) { // constant bound, rule 3
        if (i >= n) break;
        elem* s = (elem*)neon_list_at(l, (int64_t)i);
        PROVE(s == (elem*)(l->data + (size_t)i * ELEM_W.size),
              "an in-range at(i) is exactly data + i * w->size");
        PROVE(s->id == i, "and therefore reads back the element pushed into slot i");
        PROVE(s->tag == ELEM_TAG(i), "with its bytes intact, so the slot width is right");
    }

    neon_release((neon_header*)l);
    for (unsigned k = 0; k < NIDS; k++) { // constant bound, rule 3
        PROVE(live[k] == 0, "at borrows: dropping the list still releases each element once");
    }
}

int main(void) {
    // Harness dispatch only: it selects which half of the index space runs and constrains
    // no input the runtime sees. Both arms are verified in full.
    if (nondet_bool()) {
        at_outside_traps();
    } else {
        at_inside_addresses_its_slot();
    }
    return 0;
}
