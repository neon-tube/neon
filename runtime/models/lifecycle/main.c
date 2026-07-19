// Model: the reference-counting lifecycle.
//
// Drives `neon_alloc` / `neon_retain` / `neon_release` from `src/lifecycle.c` --
// the shipping source, compiled by CBMC alongside this harness -- through every
// balanced and unbalanced sequence up to a bound, asserting:
//
//   - a fresh allocation has rc == 1;
//   - rc tracks retains exactly;
//   - the object is dropped when and only when rc reaches 0;
//   - the drop runs exactly once (no double free);
//   - nothing is leaked;
//   - NULL is a no-op for both retain and release;
//   - an immortal object is never dropped, however many times it is released.
//
// The immortal case is the one worth having a machine check: a string literal
// carries NEON_IMMORTAL and is released like any other value, so "release does
// nothing" has to hold for every count, not just the ones a test happens to try.

#include "../support/cbmc_support.h"
#include "libneon_rt.h"

static unsigned drops;

static void counting_drop(void* p) {
    drops++;
    neon_free(p);
}

int main(void) {
    // NULL first: both entry points must tolerate it before anything else runs.
    neon_retain(NULL);
    neon_release(NULL);

    neon_header* h = (neon_header*)neon_alloc(8, counting_drop);
    if (h == NULL) {
        return 0; // --malloc-fail-null: nothing to verify on the OOM path.
    }
    PROVE(h->rc == 1, "a fresh allocation has rc == 1");
    drops = 0;

    unsigned retains = NONDET_UPTO(4, "four retains reach every distinct rc transition; "
                                      "the count itself is not what the code branches on");
    for (unsigned i = 0; i < retains; i++) {
        neon_retain(h);
    }
    PROVE(h->rc == 1 + retains, "rc is 1 + retains");

    // Release fewer times than the count: the object must still be alive, and
    // every field still readable -- CBMC's pointer checks catch it if not.
    unsigned early = NONDET_UPTO(retains, "releasing more than the count is a caller bug, "
                                          "not a property of this code");
    for (unsigned i = 0; i < early; i++) {
        neon_release(h);
    }
    PROVE(drops == 0, "not dropped while rc > 0");
    PROVE(h->rc == 1 + retains - early, "rc tracks releases");

    // Then the rest, taking it to zero exactly once.
    unsigned remaining = 1 + retains - early;
    for (unsigned i = 0; i < remaining; i++) {
        neon_release(h);
    }
    PROVE(drops == 1, "dropped exactly once at rc == 0");

    // An immortal object -- a string literal's buffer -- ignores release entirely.
    neon_header* lit = (neon_header*)neon_alloc(8, counting_drop);
    if (lit == NULL) {
        return 0;
    }
    lit->flags |= NEON_IMMORTAL;
    drops = 0;

    unsigned n = NONDET_UPTO(4, "the immortal path has no state, so any count exercises it");
    for (unsigned i = 0; i < n; i++) {
        neon_retain(lit);
        neon_release(lit);
    }
    for (unsigned i = 0; i < n; i++) {
        neon_release(lit);
    }
    PROVE(drops == 0, "an immortal object is never dropped");

    // It is still ours to free, so the leak check has nothing to report.
    neon_free(lit);
    return 0;
}
