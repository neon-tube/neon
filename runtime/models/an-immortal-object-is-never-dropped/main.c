// Model: an object carrying NEON_IMMORTAL driven through arbitrary retains and releases.
//
// THE INVARIANT: an object whose header carries `NEON_IMMORTAL` ignores retain and
// release entirely -- its count never moves and its drop never runs -- for any number of
// either.
//
// The motivating case is a string literal. Literal bytes live in the binary's rodata:
// there is nothing to free, and the release that frees them is not a leak the other way
// round but a segfault or a heap corruption. Codegen cannot know at a release site
// whether the `str` in hand came from a literal or from `neon_str_concat`, so it emits
// the same `neon_release` for both, and the flag is the only thing standing between a
// literal and `free()` on static storage. "Release does nothing" therefore has to hold
// for *every* count, not for the ones a test happens to try -- which is exactly the shape
// of claim a bounded model is for and an example-based test is not.
//
// The asymmetry is what makes it more than a one-liner: an immortal object that ignored
// release but honoured retain would still never be dropped, and would still be wrong --
// its count would drift upward forever with no matching decrement, and any later code
// reading `rc` to decide whether a mutation can happen in place would see a shared object
// that is in fact uniquely owned. So the model pins the count as well as the drop.
//
// Verifies `src/lifecycle.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Baseline: 127 properties, all green. Three mutations, each reverted after.
//
// 1. The `NEON_IMMORTAL` disjunct deleted from `neon_release`'s guard only. 6 of 121
//    failed, ending at "an immortal object's drop is never called, however it is
//    released" -- the release path walks the count down and calls `drop` on a static,
//    which is a free() of something malloc never returned.
//
// 2. THE ONE-SIDED CASE, and the reason this entry is worth reading: the disjunct
//    deleted from `neon_retain` ONLY, leaving release correctly guarded. The model
//    catches it -- 4 of 121, on "neither retain nor release moves an immortal object's
//    count", "retaining an immortal object leaves it uniquely owned", "releasing an
//    immortal object never decrements it...", and "and the count is where it started
//    after every operation above". Note which assertion does NOT fire: `never_dropped`
//    stays green, correctly, because the surviving release-side guard still keeps the
//    drop unreachable. So the model's strength is that it pins the COUNT and not merely
//    the absence of a drop; a model asserting only "not dropped" would have shipped
//    this one. The bug is real -- an immortal's rc drifting upward is a silent
//    divergence between the two halves of a guard that must stay symmetric.
//
// 3. `neon_alloc` publishing `h->rc = 0` -- 5 of 127, caught at
//    "the fixture starts at the count `neon_alloc` publishes" before anything else.
//
// Not caught, by design: the `--h->rc == 0` -> `h->rc-- == 0` off-by-one leaves this
// model green, since an immortal object never reaches the decrement at all. That is
// `the-drop-runs-exactly-once-at-zero`'s claim.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. NOTHING IN THE SHIPPING RUNTIME CURRENTLY SETS NEON_IMMORTAL. A string literal is
//    handled by `neon_str_lit`, which leaves `owner == NULL` rather than pointing at an
//    immortal header, and the release sites check the owner for NULL before calling
//    `neon_release` at all. So this model verifies a LATENT CONTRACT, not a live path:
//    it establishes that the flag behaves as `neon/core.h` documents it ("a NULL owner /
//    an immortal flag makes retain/release no-ops") for whenever something first sets it.
//    It is consequently NOT evidence that literals are safe today -- the owner==NULL
//    path is a different mechanism and needs its own model in the string area.
//
// 2. THE FLAG IS SET BY HAND, on an object from `neon_alloc`, which writes `flags = 0`.
//    That nothing *clears* the flag once set is not proved here; it would need a model of
//    whatever eventually sets it.
//
// 3. ONLY BIT 0 IS EXERCISED. `flags` is a `uint32_t` with one bit defined; behaviour
//    under other bits, or under `NEON_IMMORTAL` combined with a future flag, is not
//    covered, and both guards test the bit with `&` rather than comparing the word.
//
// 4. FOUR OF EACH OPERATION. The immortal path has no state -- both guards return before
//    touching a field -- so any count exercises it identically and four is chosen to
//    match the sibling models rather than for coverage.
//
// 5. THE OBJECT IS HEAP-ALLOCATED AND FREED BY HAND at the end, so `--memory-leak-check`
//    stays meaningful. A real immortal object would be static; that difference is not
//    something `neon_release` can observe, since it never reaches the storage.

#include "../support/cbmc_support.h"
#include "libneon_rt.h"

#include <stdio.h>
#include <stdlib.h>

// Rule 4. `neon_trap` calls `fflush`/`fprintf`, `neon_alloc`'s out-of-memory check
// reaches a trap under `--malloc-fail-null`, and CBMC's models of those pull a `FILE`
// into that site. The model has nothing to say about stdio.
int fprintf(FILE* stream, const char* fmt, ...) { (void)stream; (void)fmt; return 0; }
int fflush(FILE* stream) { (void)stream; return 0; }

// Reaching this at all is the failure -- and for a literal it would be `free()` on static
// storage. Naming the call is more use in a trace than a counter checked afterwards.
static void never_dropped(void* p) {
    (void)p;
    PROVE(false, "an immortal object's drop is never called, however it is released");
}

int main(void) {
    neon_header* lit = (neon_header*)neon_alloc(8, never_dropped);
    // No OOM branch to take: `neon_alloc` traps on a NULL malloc and `neon_trap` ends in
    // `_exit`, which cbmc_support.h stubs as `assume(0)`. The path is cut, not ignored.
    lit->flags |= NEON_IMMORTAL;
    PROVE(lit->rc == 1, "the fixture starts at the count `neon_alloc` publishes");

    // Balanced pairs first: a retain that is honoured and a release that is not would
    // leave the count drifting upward, which the claim after the loop catches.
    unsigned pairs = NONDET_UPTO(4, "the immortal path has no state -- both guards return "
                                    "before reading a field -- so any count exercises it");
    for (unsigned i = 0; i < 4; i++) { // constant bound, rule 3
        if (i >= pairs) break;
        neon_retain(lit);
        neon_release(lit);
        PROVE(lit->rc == 1, "neither retain nor release moves an immortal object's count");
    }

    // Then unmatched retains, which must not move the count either.
    unsigned extra_retains = NONDET_UPTO(4, "same reason: the path is stateless");
    for (unsigned i = 0; i < 4; i++) { // constant bound, rule 3
        if (i >= extra_retains) break;
        neon_retain(lit);
    }
    PROVE(lit->rc == 1, "retaining an immortal object leaves it uniquely owned");

    // Then unmatched releases -- more of them than there were retains, so an object that
    // honoured either guard's other arm would have been dropped by now.
    unsigned extra_releases = NONDET_UPTO(4, "same reason: the path is stateless");
    for (unsigned i = 0; i < 4; i++) { // constant bound, rule 3
        if (i >= extra_releases) break;
        neon_release(lit);
        PROVE(lit->rc == 1, "releasing an immortal object never decrements it, so it "
                            "cannot reach zero however many releases arrive");
    }
    PROVE(lit->rc == 1, "and the count is where it started after every operation above");
    PROVE(lit->flags & NEON_IMMORTAL, "neither entry point clears the flag it tested");
    PROVE(lit->drop == never_dropped, "nor disturbs the drop it declined to call");

    // Still ours: the runtime never took ownership, so the leak check has nothing to
    // report only because the harness frees it (SCOPE 5).
    neon_free(lit);
    return 0;
}
