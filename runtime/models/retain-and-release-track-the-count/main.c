// Model: retains and releases against a live object, observing `rc` after each.
//
// THE INVARIANT: while an object is alive, `rc` is exactly `1 + retains - releases`,
// and NULL is a no-op for both entry points.
//
// The arithmetic looks too small to be worth a machine check, and the count itself is:
// `rc++` is hard to get wrong. What is worth checking is that nothing else on the path
// touches it. Both entry points open with a compound guard --
// `h == NULL || (h->flags & NEON_IMMORTAL)` -- and every codegen-emitted retain and
// release in the language funnels through those two lines. A guard that returned early
// on an ordinary object, or one that fell through on NULL, would be a null dereference
// or a leak in every program the compiler emits, and there is no other place that
// rechecks it. Driving the pair over every reachable count, with `--pointer-check` on,
// is what turns "the guard is right" from a reading of four lines into a proof.
//
// The count is the *observable* here, not a side effect: `neon_alloc` publishes `rc == 1`
// and this model pins that too, because "1 + retains" is only a claim if the 1 is one.
//
// The object is never taken to zero. That is deliberate and is the whole of rule 2's
// argument: the drop transition is a separate contract with its own failure mode, and it
// has its own model (`the-drop-runs-exactly-once-at-zero`). Keeping the drop unreachable
// here is also what keeps this model sub-second -- see the README's note on how a
// symbolically-freed object poisons every later dereference.
//
// Verifies `src/lifecycle.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Baseline: 126 properties, all green. Three mutations, each reverted after.
//
// 1. The `h == NULL` disjunct deleted from the guard in BOTH `neon_retain` and
//    `neon_release`, leaving only the immortal test -- the tidy-up that reads like
//    dead code removal because callers "obviously" hold a real pointer. 23 of 126
//    failed, and the shape of the failure is the point: not one assertion but the
//    whole dereference chain, `pointer NULL in h->flags` at both entry points and
//    on through `h->rc` and the indirect call to `h->drop`. Every codegen-emitted
//    retain and release funnels through those two lines, so this is a segfault in
//    any program that lets an optional reach the end of its scope.
//
// 2. `h->rc++` changed to `h->rc += 2` in `neon_retain` -- the double-count that a
//    hand-merged retain would produce. Failed on "rc is 1 + retains" and
//    "rc is 1 + retains - releases", 4 of 126. Costs nothing at runtime and leaks
//    every object it touches, which is exactly why no test would find it.
//
// 3. `neon_alloc` publishing `h->rc = 0` instead of 1 -- the off-by-one that treats
//    the returned pointer as unowned. Failed on "a fresh allocation is uniquely
//    owned: rc is 1" and five more, 7 of 126, including "an object whose count never
//    reached zero is never dropped": the first release wraps to UINT64_MAX rather
//    than freeing, so the header's `1 +` really is load-bearing.
//
// Not caught, by design: `--h->rc == 0` -> `h->rc-- == 0` leaves this model green
// (SCOPE 1 -- the zero transition is never reached here). That mutation is caught by
// `the-drop-runs-exactly-once-at-zero`, which is why the split exists.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. THE ZERO TRANSITION IS NOT REACHED. Nothing here proves the object is dropped when
//    the count runs out, nor that the drop runs once; `releases <= retains` is assumed
//    precisely so the count stays at or above 1. That contract belongs to
//    `the-drop-runs-exactly-once-at-zero`.
//
// 2. OVER-RELEASE IS NOT A MODELLED STATE. Releasing more times than the object was
//    retained is a caller bug -- a use-after-free the refcount pass in codegen is
//    responsible for not emitting -- and `neon_release` has no defence against it by
//    design. So this model says nothing about what happens after one, and in particular
//    does not prove any diagnostic fires.
//
// 3. FOUR RETAINS, NOT ARBITRARILY MANY. `rc` is a `uint64_t` and the code branches on
//    the *result* of the decrement rather than on the count, so four is enough to reach
//    every distinct transition it has. Saturation at UINT64_MAX is consequently not
//    covered: `neon_retain` does not check for it, and an object reaching 2^64 references
//    is not a state this runtime can be driven into.
//
// 4. IMMORTAL OBJECTS TAKE THE OTHER ARM OF THE SAME GUARD and are not touched here; see
//    `an-immortal-object-is-never-dropped`. The `flags` field is left at the 0 that
//    `neon_alloc` writes.
//
// 5. SINGLE-THREADED. `rc` is a plain `uint64_t` incremented non-atomically, so nothing
//    here is evidence about concurrent retain/release. That is a property of the runtime's
//    design, not of the model.
//
// ---- Assumptions ----
//
// `releases <= retains` (SCOPE 2) is the one assumption beyond bound-encoding. It is what
// keeps the object alive for the duration, which is the premise of the invariant rather
// than a case being ducked.

#include "../support/cbmc_support.h"
#include "libneon_rt.h"

#include <stdio.h>
#include <stdlib.h>

// Rule 4. `neon_trap` calls `fflush`/`fprintf`, `neon_alloc`'s out-of-memory check
// reaches a trap under `--malloc-fail-null`, and CBMC's models of those pull a `FILE`
// into that site. The model has nothing to say about stdio.
int fprintf(FILE* stream, const char* fmt, ...) { (void)stream; (void)fmt; return 0; }
int fflush(FILE* stream) { (void)stream; return 0; }

// The object under test never reaches zero, so reaching this at all is the failure.
// Stronger than counting drops and asserting the count is 0: this names the call.
static void never_dropped(void* p) {
    (void)p;
    PROVE(false, "an object whose count never reached zero is never dropped");
}

int main(void) {
    // Both entry points must tolerate NULL before anything else runs. There is no
    // counter to read afterwards -- the property is that neither dereferenced, which
    // `--pointer-check` is the oracle for.
    neon_retain(NULL);
    neon_release(NULL);

    neon_header* h = (neon_header*)neon_alloc(8, never_dropped);
    // No OOM branch to take: `neon_alloc` traps on a NULL malloc and `neon_trap` ends in
    // `_exit`, which cbmc_support.h stubs as `assume(0)`. The path is cut, not ignored.
    PROVE(h->rc == 1, "a fresh allocation is uniquely owned: rc is 1");
    PROVE(h->flags == 0, "and carries no flags, so neither guard's second arm is taken");
    PROVE(h->drop == never_dropped, "and keeps the drop function it was given");

    unsigned retains = NONDET_UPTO(4, "four retains reach every distinct rc transition; "
                                      "the code branches on the decrement's result, not "
                                      "on the count");
    for (unsigned i = 0; i < 4; i++) { // constant bound, rule 3
        if (i >= retains) break;
        neon_retain(h);
    }
    PROVE(h->rc == 1 + retains, "rc is 1 + retains");

    // Interleaving a NULL retain here, rather than only at the top, is what makes the
    // guard's arms distinguishable: a `neon_retain` that ignored its argument entirely
    // would pass the calls above and fail the count below.
    neon_retain(NULL);
    PROVE(h->rc == 1 + retains, "retaining NULL touches no other object's count");

    unsigned releases = NONDET_UPTO(4, "matched to the retain bound above");
    ASSUME(releases <= retains,
           "the premise of the invariant, not a case ducked: releasing more times than "
           "the object was retained is a caller bug (SCOPE 2), and the claim being made "
           "here is about an object that is still alive");
    for (unsigned i = 0; i < 4; i++) { // constant bound, rule 3
        if (i >= releases) break;
        neon_release(h);
    }
    PROVE(h->rc == 1 + retains - releases, "rc is 1 + retains - releases");
    PROVE(h->rc >= 1, "and the object is still alive, so every field above was readable");

    neon_release(NULL);
    PROVE(h->rc == 1 + retains - releases, "releasing NULL touches no other object's count");

    // By hand, not `neon_release`: taking it to zero is the next model's contract, and
    // the leak check wants this back.
    neon_free(h);
    return 0;
}
