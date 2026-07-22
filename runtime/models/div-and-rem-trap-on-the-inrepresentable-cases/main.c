// Model: `neon_i64_div` and `neon_i64_rem`, in two passes -- an unconstrained pair for the
// trap side and a concrete boundary table for the returning side.
//
// THE INVARIANT: division and remainder trap on exactly the two inputs whose result C
// leaves undefined -- a zero divisor, and `INT64_MIN / -1` whose true quotient is not
// representable -- and on every other input they return the truncated-toward-zero quotient
// and remainder, which satisfy `q * b + r == a` with `r` carrying the dividend's sign.
//
// Unlike `+`/`-`/`*`, these do NOT wrap: `a / b` and `a % b` are compiled straight, and
// the two `if` guards ahead of them are the whole defence against the two cases where that
// division is undefined behaviour. The model does not assert "it traps" directly -- a trap
// ends in `_exit`, which the support header cuts with `assume(0)`, so a trapping input
// simply has no path to any assertion.
//
// PASS 1 -- the trap side, over an unconstrained (a, b). What proves the guards are
// *sufficient* is the verifier's own `--div-by-zero-check` and `--signed-overflow-check`
// running on the shipped `a / b`: if either guard were missing, its input would still reach
// the bare division and CBMC would flag the UB there, for EVERY input, not a sampled one.
// So the built-in checks are the oracle for the trap side. The one value property that is
// cheap at full width -- the remainder's sign -- is asserted here too; the quotient's
// *value* is not, because a symbolic 64-bit division does not bit-blast in tractable time
// (measured: the identity `q*b+r==a` over a symbolic pair does not finish in 120s, while
// this pass finishes in well under a second). See the README on solve time.
//
// PASS 2 -- the returning side, over a concrete table. The value the returning path yields
// is C's `/` and `%`, and pinning it needs the full quotient, so pass 2 drives a handful of
// literal (a, b) pairs -- every sign combination and the INT64 magnitude boundaries -- where
// the division constant-folds and `q == a/b`, `r == a%b` and the reconstruction identity are
// all checkable at no solver cost. The two trapping inputs are deliberately absent here:
// calling `neon_i64_div(a, 0)` would cut the path and make the rest of the pass vacuous, so
// the trap side is pass 1's job and pass 2 only ever divides where the result is defined.
//
// Verifies `src/arith.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Baseline: 15 properties, VERIFICATION SUCCESSFUL. Five mutations, each reverted after.
//
// 1. `neon_i64_div` losing both its guards, so `a / b` runs bare. Failed 2 of 15 on the
//    built-in "division by zero in a / b" and "signed division overflow in a / b" -- pass
//    1's oracle, and the proof the guards are load-bearing for every input, not a sample.
//
// 2. `neon_i64_div` losing only the `INT64_MIN / -1` guard. Failed 1 of 15 on "signed
//    division overflow in a / b" -- the one input the remaining zero-guard does not cover.
//
// 3. `neon_i64_div` off by one (`a / b + 1`). Failed 4 of 16 on "div returns the
//    truncated-toward-zero quotient" and the reconstruction identity from pass 2's table --
//    a value bug the trap side cannot see, which is why pass 2 exists.
//
// 4. `neon_i64_rem` returning the quotient (`a / b`) instead of `a % b`. Failed 4 of 15 on
//    "rem returns the matching remainder", the identity, and pass 1's remainder-sign claim.
//
// 5. `neon_i64_rem` losing its guards. Failed 2 of 15 on "division by zero in a % b" and
//    "signed mod not representable in a % b". This is the mutation the FIRST cut of the
//    model missed: div and rem were driven on one shared pair, so div's own guard trapped
//    and cut the path before rem's missing guard could be reached, and the mutant passed
//    silently. Splitting pass 1 into independent operands per function is what closes it --
//    a masking gap the README's rule 6 ("a passing model is not a result until you have
//    seen it fail") is exactly about.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. THE TRAP MESSAGE AND EXIT are not observed: a trap is a cut path here (`_exit` ->
//    `assume(0)`), so this says nothing about *what* a trap prints or its exit code, only
//    that the two undefined inputs cannot reach the division. `neon_trap` itself is
//    exercised by `list-at-traps-outside-the-list`.
//
// 2. THE QUOTIENT VALUE IS PINNED ONLY AT THE TABLE'S POINTS. Pass 1 proves no input
//    reaches UB and fixes the remainder's sign for all of them, but the quotient's exact
//    value is checked only at pass 2's concrete pairs. A value bug that spares every sign
//    boundary and every INT64 extreme in the table would not be seen -- the table is chosen
//    to leave no such gap for the two-operator, no-branch bodies these functions have.
//
// 3. THE WRAPPING OPERATORS ARE ELSEWHERE. `+`, `-`, `*` and unary `-` wrap rather than
//    trap and are a different contract with a different oracle; they are not in this model.
//
// 4. SINGLE 64-BIT WIDTH. `i64` is the only integer type with these entry points.

#include "../support/cbmc_support.h"
#include "libneon_rt.h"

#include <stdio.h>

// Rule 4. A missing guard reaches `neon_trap`, which calls `fflush`/`fprintf`; CBMC's
// models of those pull a `FILE` into the site. The model has nothing to say about stdio.
int fprintf(FILE* stream, const char* fmt, ...) { (void)stream; (void)fmt; return 0; }
int fflush(FILE* stream) { (void)stream; return 0; }

int64_t nondet_i64(void);

// PASS 2's per-pair checker. Called only with literal arguments, so every division here
// constant-folds: the full quotient is pinned at no solver cost. `b` is never 0 and the
// pair is never (INT64_MIN, -1), so neither call traps and the path is not cut.
static void divides_correctly(int64_t a, int64_t b) {
    int64_t q = neon_i64_div(a, b);
    int64_t r = neon_i64_rem(a, b);
    PROVE(q == a / b, "div returns the truncated-toward-zero quotient");
    PROVE(r == a % b, "rem returns the matching remainder");
    PROVE(q * b + r == a, "quotient and remainder reconstruct the dividend: q*b + r == a");
}

int main(void) {
    // ---- PASS 1: the trap side, unconstrained ----
    //
    // div and rem are driven on SEPARATE unconstrained pairs, not one shared pair. Sharing
    // masks a bug: div is called first, so on a shared (a, b) its own guard traps and cuts
    // the path on exactly the inputs that would expose a missing guard in rem. Independent
    // operands make each function's guards the only thing standing between its input and the
    // bare division -- which is what the built-in checks are then the oracle for.

    // div's trap side. Reaching past the call lets `--div-by-zero-check` and
    // `--signed-overflow-check` pass on the shipped `a / b` for every non-trapping input.
    int64_t ad = nondet_i64();
    int64_t bd = nondet_i64();
    int64_t q = neon_i64_div(ad, bd);
    (void)q; // the quotient's value is pass 2's job; pass 1 only proves it is reachable
    ASSUME(bd != 0, "a zero divisor traps in div, so its returning path has bd != 0");
    ASSUME(!(ad == INT64_MIN && bd == -1),
           "INT64_MIN / -1 traps in div, so its returning path excludes it too");

    // rem's trap side, independently. The remainder's sign is the one value property cheap
    // enough to pin at full width, so it rides along here.
    int64_t ar = nondet_i64();
    int64_t br = nondet_i64();
    int64_t r = neon_i64_rem(ar, br);
    ASSUME(br != 0, "a zero divisor traps in rem, so its returning path has br != 0");
    ASSUME(!(ar == INT64_MIN && br == -1),
           "INT64_MIN / -1 traps in rem, so its returning path excludes it too");
    PROVE(r == 0 || (r < 0) == (ar < 0),
          "the remainder is truncation toward zero: it carries the dividend's sign");

    // ---- PASS 2: the returning side, concrete ----
    // Every sign combination, then the INT64 magnitude boundaries -- including the
    // dividend INT64_MIN with a divisor other than -1, whose quotient is representable.
    divides_correctly(0, 1);
    divides_correctly(7, 3);
    divides_correctly(-7, 3);
    divides_correctly(7, -3);
    divides_correctly(-7, -3);
    divides_correctly(INT64_MAX, 2);
    divides_correctly(INT64_MAX, INT64_MIN);
    divides_correctly(INT64_MIN, 2);
    divides_correctly(INT64_MIN, 1);
    divides_correctly(INT64_MIN, INT64_MAX);
    divides_correctly(INT64_MIN, INT64_MIN);
    divides_correctly(5, INT64_MIN);
    return 0;
}
