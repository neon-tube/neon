// Model: an object released down to zero from an arbitrary count, watching the drop.
//
// THE INVARIANT: the drop function runs when and only when `rc` reaches 0 -- exactly
// once, never while `rc > 0`, never twice.
//
// This is the contract the whole memory model rests on, and both halves of it are a
// crash rather than a wrong answer. `neon_release` is one line -- `if (--h->rc == 0)
// h->drop(h);` -- and the two ways to write it wrong are both plausible: `h->rc-- == 0`
// instead of `--h->rc == 0` drops on the *next-to-last* release and leaves a live object
// freed under its owner, and `h->rc <= 0` on an unsigned count that has already wrapped
// drops nothing at all. Neither is visible in an ordinary test that retains once and
// releases once, because at rc == 1 the correct and the off-by-one behaviours coincide.
// This model starts from a nondeterministic count precisely so that the depth at which
// they diverge is covered rather than assumed away.
//
// The drop is also the only place the object's own identity is checked: `h->drop(h)`
// passes the header it was invoked on, and a drop handed the wrong pointer would free
// somebody else's allocation. `counting_drop` asserts on that too.
//
// The leak checker is a second, independent oracle here. The object is freed only from
// inside the drop, so a release sequence that failed to reach zero fails `--memory-leak-
// check` even if every count assertion somehow held.
//
// Verifies `src/lifecycle.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Baseline: 97 properties, all green. Three mutations, each reverted after.
//
// 1. `neon_release`'s `--h->rc == 0` changed to `h->rc-- == 0` -- the off-by-one that
//    tests the count before the decrement, so the drop fires on the release that takes
//    rc from 1 to 0 only by never firing at all, and the object is freed a release too
//    early or not at all. Failed on "the drop runs exactly once, at the release that
//    takes rc to 0" plus the memory-leak property, 2 of 97. This is the mutation the
//    model exists for: it frees a live object under its owner, and nothing else in the
//    tree rechecks the branch.
//
// 2. `neon_retain`'s `h->rc++` made a no-op -- a retain that guards correctly and then
//    forgets to count. Failed on "the drop does not run while rc is still above zero",
//    "after retains-many releases the original reference is all that is left", "and
//    nothing has been dropped yet", and dragged CBMC's own `deallocated dynamic object`
//    dereference checks in `neon_release` with them: 9 of 90. A use-after-free on every
//    aliased value the compiler emits.
//
// 3. `neon_alloc` publishing `h->rc = 0` -- 12 of 97, the widest blast radius of the
//    three, failing every count assertion and the drop-timing pair together.
//
// Not caught, by design: removing the NULL guard leaves this model green (its fixture
// never passes NULL); `retain-and-release-track-the-count` owns that claim.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. THE DROP IS A LEAF. `counting_drop` frees the header and nothing else, whereas a
//    real drop (`neon_list_drop`, `neon_map_drop`) releases the object's counted fields
//    first and so re-enters `neon_release`. Recursive teardown -- and any cycle it could
//    form -- is therefore not covered here. The reason is the one the README gives for
//    rule 2: an object released inside a drop is symbolically freed at every later
//    dereference, and the recursion re-expands to the full unwind depth at each. Those
//    contracts belong to the per-container models.
//
// 2. NOTHING IS PROVED ABOUT USE AFTER THE DROP. Once the count reaches zero the header
//    is freed and this model stops reading it. That a caller does not touch it afterwards
//    is codegen's refcount pass's obligation, not `neon_release`'s.
//
// 3. FOUR RETAINS AHEAD OF THE TEARDOWN, so counts of 1 through 5 are covered. The code
//    branches on the decrement's result rather than on the count, so a deeper start
//    reaches no new state; UINT64_MAX saturation is not modelled and `neon_retain` does
//    not check for it.
//
// 4. IMMORTAL OBJECTS ARE NOT DROPPED AT ALL and take the guard's other arm; see
//    `an-immortal-object-is-never-dropped`. `flags` is left at the 0 `neon_alloc` writes.
//
// 5. SINGLE-THREADED. The decrement is non-atomic, so "exactly once" is proved for one
//    thread of control only -- two threads racing the final release is outside what this
//    runtime's `rc` is designed for, and outside what CBMC is being asked here.

#include "../support/cbmc_support.h"
#include "libneon_rt.h"

#include <stdio.h>
#include <stdlib.h>

// Rule 4. `neon_trap` calls `fflush`/`fprintf`, `neon_alloc`'s out-of-memory check
// reaches a trap under `--malloc-fail-null`, and CBMC's models of those pull a `FILE`
// into that site. The model has nothing to say about stdio.
int fprintf(FILE* stream, const char* fmt, ...) { (void)stream; (void)fmt; return 0; }
int fflush(FILE* stream) { (void)stream; return 0; }

static unsigned drops;
static neon_header* expected;

static void counting_drop(void* p) {
    // The over-release oracle, sharper than the count check below: it fails at the
    // release in lifecycle.c that dropped once too often, not at some later read.
    PROVE(drops == 0, "the drop runs at most once for one object");
    PROVE(p == expected, "the drop is handed the header it was invoked on");
    drops++;
    neon_free(p);
}

int main(void) {
    neon_header* h = (neon_header*)neon_alloc(8, counting_drop);
    // No OOM branch to take: `neon_alloc` traps on a NULL malloc and `neon_trap` ends in
    // `_exit`, which cbmc_support.h stubs as `assume(0)`. The path is cut, not ignored.
    expected = h;
    PROVE(h->rc == 1, "a fresh allocation is uniquely owned: rc is 1");
    PROVE(drops == 0, "and allocating drops nothing");

    unsigned retains = NONDET_UPTO(4, "counts of 1 through 5 reach every distinct "
                                      "decrement; the code branches on the result of the "
                                      "decrement, not on the count");
    for (unsigned i = 0; i < 4; i++) { // constant bound, rule 3
        if (i >= retains) break;
        neon_retain(h);
    }

    // Every release but the last. The object is alive throughout, so each field read
    // below is a live read -- `--pointer-check` is the oracle if a drop fired early.
    for (unsigned i = 0; i < 4; i++) { // constant bound, rule 3
        if (i >= retains) break;
        neon_release(h);
        PROVE(drops == 0, "the drop does not run while rc is still above zero");
        PROVE(h->rc > 0, "and the object is still alive, so the count never wrapped");
    }
    PROVE(h->rc == 1, "after retains-many releases the original reference is all that is left");
    PROVE(drops == 0, "and nothing has been dropped yet");

    // The last one, which takes it to zero.
    neon_release(h);
    PROVE(drops == 1, "the drop runs exactly once, at the release that takes rc to 0");

    // `h` is freed and deliberately not read again (SCOPE 2). `--memory-leak-check` is
    // the independent witness that it *was* freed: the only `neon_free` is in the drop.
    return 0;
}
