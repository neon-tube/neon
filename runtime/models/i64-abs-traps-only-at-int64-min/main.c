// Model: `neon_i64_abs` over an unconstrained i64.
//
// THE INVARIANT: `neon_i64_abs` traps on exactly one input -- `INT64_MIN`, whose magnitude
// is not representable as a positive i64 -- and on every other input returns a non-negative
// value equal to the operand's magnitude.
//
// `i64` deliberately does NOT follow `f64`'s IEEE "just compute it" rule: where `sqrt(-1)`
// is a quiet NaN, `abs(INT64_MIN)` has no answer an i64 can hold, so it traps, exactly as
// division does at `INT64_MIN / -1`. The single guard `if (a == INT64_MIN)` is the whole of
// that, and the body's `-a` is signed negation -- undefined at `INT64_MIN`. As with div,
// the model does not assert "it traps"; the trap ends in `_exit` (cut by `assume(0)`), so a
// trapping input has no path to any assertion, and the verifier's own `--signed-overflow-
// check` running on the shipped `-a` is what proves the guard is *sufficient*: delete it and
// `INT64_MIN` reaches the bare negation and CBMC flags the UB there.
//
// The operand is fully unconstrained, so this is neither sampled nor bounded -- there is no
// loop and no allocation, and the only `ASSUME` restates the reached path's precondition
// that the (cut) trap already guarantees.
//
// Verifies `src/math.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Baseline: 6 properties, VERIFICATION SUCCESSFUL. Three mutations, each reverted after.
//
// 1. The `INT64_MIN` guard deleted, so `-a` runs bare. Failed 1 of 6 on the built-in
//    "signed unary minus overflow in -a" -- the oracle, and the proof the guard is what
//    keeps the one undefined input away from the negation, for every input rather than a
//    sampled one.
//
// 2. Negating the wrong arm (`a > 0 ? -a : a`), so positives come back negative. Failed
//    2 of 6 on "abs is non-negative" and "abs is the operand's magnitude".
//
// 3. Never negating (`return a`). Failed 2 of 5 on the same two claims -- a negative
//    operand is returned unchanged.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. THE TRAP MESSAGE AND EXIT are not observed: the trap is a cut path (`_exit` ->
//    `assume(0)`), so this proves only that `INT64_MIN` cannot reach the negation, not what
//    the trap prints or exits with. `neon_trap` itself is exercised elsewhere.
//
// 2. THE f64 MATH IS NOT HERE. `neon_f64_sqrt` and friends are thin passes over libm that
//    do not trap (an out-of-domain input is a NaN or infinity by design), so there is no
//    guard to prove and modelling them would verify libm, not this runtime (rule 4). The
//    trapping f64 conversion is `neon_f64_to_i64`, whose out-of-range guard has its own
//    model.
//
// 3. SINGLE 64-BIT WIDTH. `i64` is the only integer type with this entry point.

#include "../support/cbmc_support.h"
#include "libneon_rt.h"

#include <stdio.h>

// Rule 4. A missing guard reaches `neon_trap`, which calls `fflush`/`fprintf`; CBMC's
// models of those pull a `FILE` into the site. The model has nothing to say about stdio.
int fprintf(FILE* stream, const char* fmt, ...) { (void)stream; (void)fmt; return 0; }
int fflush(FILE* stream) { (void)stream; return 0; }

int64_t nondet_i64(void);

int main(void) {
    int64_t a = nondet_i64();

    // Traps -- and so cuts the path -- iff a == INT64_MIN. Reaching the next line is what
    // lets `--signed-overflow-check` pass on the shipped `-a` for every other input.
    int64_t r = neon_i64_abs(a);

    // Restates the precondition the cut trap enforces: documentation of the reached state,
    // and the record that the guard, not luck, removed the one undefined input. With it
    // held, the harness's own `-a` below is itself defined.
    ASSUME(a != INT64_MIN, "INT64_MIN traps, so the returning path excludes it");

    PROVE(r >= 0, "abs is non-negative");
    PROVE(r == (a < 0 ? -a : a), "abs is the operand's magnitude");

    // Idempotent on its own output, which is non-negative and so never INT64_MIN: a second
    // abs cannot trap and must be the identity. Catches a body that negates the wrong arm.
    PROVE(neon_i64_abs(r) == r, "abs of a non-negative value is that value");
    return 0;
}
