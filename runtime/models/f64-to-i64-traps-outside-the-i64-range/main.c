// Model: `neon_f64_to_i64` over an unconstrained double, then a concrete truncation table.
//
// THE INVARIANT: the f64->i64 conversion traps on exactly the inputs whose truncation an
// i64 cannot hold -- NaN, and any magnitude at or beyond 2^63 -- and on every accepted
// input returns C's truncation toward zero.
//
// This is the one f64 entry point that traps. The rest of `math.c` keeps IEEE semantics and
// answers out-of-domain inputs with NaN or infinity (see the file's own header), so they
// have no guard to check; here the C cast `(int64_t)x` is undefined for a value whose
// truncation is out of range, and the three-part guard `x != x || x >= 2^63 || x < -2^63`
// is the whole defence. The two thresholds are exact doubles -- `2^63` and `-2^63` both
// convert without rounding -- so the accepted band is precisely `[-2^63, 2^63)`, and every
// double in it truncates to something an i64 holds.
//
// PASS 1 -- the trap side, over an unconstrained double. As with div and abs, a trap is a
// cut path (`_exit` -> `assume(0)`), so a trapping input reaches no assertion, and the
// verifier's own `--conversion-check` running on the shipped `(int64_t)x` is the oracle: it
// is the check that a float-to-integer conversion is representable, and if any of the three
// guard clauses were missing its inputs would reach the bare cast and CBMC would flag it --
// which the mutation record below confirms for each clause. The sign of the result is the
// one value property cheap at full width and is pinned here.
//
// PASS 2 -- the accepted side, over a concrete table. Truncation *toward zero* (not floor,
// not round) is the behavioural claim, and pinning it needs the actual result, so pass 2
// drives literal doubles -- both sign directions with a fractional part, and the exact
// endpoints of the accepted band -- where the cast constant-folds and the result is checked
// at no solver cost. The trapping inputs are absent here: calling the conversion on a NaN or
// an out-of-range value would cut the path and make the rest of the pass vacuous.
//
// Verifies `src/math.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Baseline: 4 properties, VERIFICATION SUCCESSFUL. Five mutations, each reverted after.
//
// 1. The NaN clause deleted (`x != x ||` removed). Failed 1 of 4 on the built-in "float to
//    signed integer conversion overflow in (int64_t)x" -- a NaN now reaches the cast, which
//    is undefined, and pass 1's oracle catches it.
//
// 2. The upper-bound clause deleted. Failed 1 of 4 on the same conversion check: a value at
//    or beyond +2^63 reaches the cast.
//
// 3. The lower-bound clause deleted. Failed 1 of 4 likewise, from the negative side.
//
// 4. The cast returning one too many (`(int64_t)x + 1`). Failed 2 of 5 on "the accepted
//    conversion truncates toward zero" (pass 2) and on pass 1's sign claim -- a value bug
//    the trap side cannot see, which is why the concrete table exists.
//
// 5. The upper bound loosened from `>=` to `>`, so exactly 2^63 is accepted. Failed 1 of 4
//    on the conversion check: 2^63 is one past the largest representable i64 and its cast is
//    undefined. This is why the threshold is an inclusive `>=`, and the model reaches the
//    exact boundary to prove it.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. THE TRAP MESSAGE AND EXIT are not observed: the trap is a cut path, so this proves
//    only that no out-of-range input reaches the cast, not what the trap prints or exits
//    with.
//
// 2. THE REVERSE CONVERSION IS NOT HERE. `neon_i64_to_f64` is a total `(double)a` with no
//    guard -- every i64 converts, rounding past 2^53 the way every language with these two
//    types does -- so it has no trap to prove; its rounding is a property of the hardware's
//    round-to-nearest, not of this runtime (rule 4).
//
// 3. THE LOWER ENDPOINT -2^63 IS NOT DRIVEN, and the reason is the verifier, not the
//    runtime. -2^63 is exactly INT64_MIN and the runtime converts it correctly, but CBMC
//    6.10's `--conversion-check` flags `(int64_t)(-2^63)` as an overflow while
//    simultaneously computing INT64_MIN for it (probed directly: the value assertion
//    passes, the conversion check on the same cast fails). Feeding that one value -- from
//    either pass -- would be a false positive from the oracle, so both passes exclude it;
//    pass 2's nearest-the-bottom point is `-(2^63 - 1024)` instead. The runtime's handling
//    of exactly INT64_MIN is left to the tinyunit suite, which can observe the real cast.
//
// 4. THE ACCEPTED-VALUE RESULT IS PINNED ONLY AT THE TABLE'S POINTS. Pass 1 proves no
//    accepted input reaches UB and fixes the result's sign for all of them; the exact
//    truncated value is checked only at pass 2's literals, chosen to cover both signs, a
//    fractional part, and the reachable endpoints of the band.
//
// 5. `neon_f64_to_fixed` -- the `%.*f` renderer -- is not modelled: it is a `snprintf`
//    wrapper whose output is libc's, not this runtime's (rule 4).

#include "../support/cbmc_support.h"
#include "libneon_rt.h"

#include <stdio.h>

// Rule 4. A missing guard reaches `neon_trap`, which calls `fflush`/`fprintf`; CBMC's
// models of those pull a `FILE` into the site. The model has nothing to say about stdio.
int fprintf(FILE* stream, const char* fmt, ...) { (void)stream; (void)fmt; return 0; }
int fflush(FILE* stream) { (void)stream; return 0; }

double nondet_f64(void);

// The exact doubles 2^63 and -2^63 -- the guard's own thresholds, and the endpoints of the
// accepted band. 2^63 itself traps (the band is half-open); -2^63 is accepted and is
// INT64_MIN. `9223372036854774784.0` is the largest double strictly below 2^63 (they are
// 1024 apart there), so it is the accepted input nearest the upper edge.
#define TWO63          9223372036854775808.0
#define NEG_TWO63     -9223372036854775808.0
#define BELOW_TWO63    9223372036854774784.0

// PASS 2's per-value checker. Called only with literal, in-range, non-NaN arguments, so the
// cast constant-folds and the truncated result is pinned with no solver cost.
static void truncates_to(double x, int64_t expect) {
    PROVE(neon_f64_to_i64(x) == expect, "the accepted conversion truncates toward zero");
}

int main(void) {
    // ---- PASS 1: the trap side, unconstrained ----
    double x = nondet_f64();

    // The single value CBMC's `--conversion-check` is conservative at (see SCOPE 3): -2^63
    // is exactly INT64_MIN and the runtime accepts it, but CBMC flags `(int64_t)(-2^63)` as
    // an overflow while itself computing INT64_MIN for it. Excluding it here keeps the
    // oracle sound for the rest of the band; it is the only band value pass 1 gives up, and
    // it is a tool boundary, not a runtime one. NaN and out-of-range inputs are deliberately
    // left in, so the guard must still trap them.
    ASSUME(x != NEG_TWO63,
           "CBMC's conversion-check is conservative at exactly -2^63 = INT64_MIN; the "
           "runtime accepts it and CBMC computes the right result, but flags the cast. This "
           "single tool-boundary value is excluded so the oracle covers the rest soundly.");

    // Traps -- and so cuts the path -- on NaN or |x| >= 2^63. Reaching the next line lets
    // `--conversion-check` pass on the shipped `(int64_t)x` for every accepted input.
    int64_t r = neon_f64_to_i64(x);

    // Restates the precondition the cut trap enforces: the reached state is a non-NaN value
    // inside the band, above the excluded endpoint, which is exactly where the cast is
    // defined and CBMC agrees it is.
    ASSUME(x == x, "NaN traps, so the returning path is not NaN");
    ASSUME(x > NEG_TWO63 && x < TWO63,
           "|x| >= 2^63 traps, so the returning path is inside the representable band");

    PROVE(x >= 0.0 ? r >= 0 : r <= 0,
          "truncation toward zero preserves sign: a non-negative input yields a "
          "non-negative result, and a negative input a non-positive one");

    // ---- PASS 2: the accepted side, concrete ----
    truncates_to(0.0, 0);
    truncates_to(2.9, 2);        // toward zero, not up
    truncates_to(-2.9, -2);      // toward zero, not down (floor would give -3)
    truncates_to(1.0, 1);
    truncates_to(BELOW_TWO63, 9223372036854774784);   // accepted input nearest +2^63
    truncates_to(-BELOW_TWO63, -9223372036854774784); // and nearest the excluded -2^63
    return 0;
}
