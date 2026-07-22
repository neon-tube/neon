// Model: `neon_box_expect` over an unconstrained (stamped, asked) tag pair.
//
// THE INVARIANT: `neon_box_expect` returns only when the asked tag equals the tag the box
// was stamped with, and what it returns is the box's own payload. On any other pair it
// traps -- and a trap is a cut path (`_exit` -> `assume(0)`), so a mismatched pair that
// *returned* would flow straight into the assertions below and fail them.
//
// This is the runtime half of `as`-from-`any` (docs/design/opacity.md, residue 1): the
// cast out of `any` asserts the box holds the target type, and this function is where
// that assertion is discharged. The property that matters is the implication "returned
// => tags equal": it is exactly what stops a structural `{ code: 99 }` boxed into `any`
// from being read back as an opaque record -- or a boxed `str`'s pointer word being read
// as an `i64` -- because the wrong-tag read never happens; the path dies in the trap.
//
// Both tags are unconstrained, so the proof covers the full 2^128 pair space: equal pairs
// must reach the assertions and satisfy them, unequal pairs must trap. The payload is a
// concrete marker word: copy fidelity, ownership and drop behaviour are the round-trip
// model's property (box-round-trips-its-payload-and-drops-it-once), not this one's --
// rule 2, one model, one invariant.
//
// Verifies `src/any.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Baseline: 200 properties, VERIFICATION SUCCESSFUL. Three mutations, each reverted after.
//
// 1. The check deleted (`neon_box_expect` returns unconditionally) -- the pre-fix runtime,
//    where the cast read the payload without looking at the tag. Failed 1 of 194 on "a
//    returning neon_box_expect means the asked tag is the tag the box was stamped with":
//    a mismatched pair now reaches the assertion.
//
// 2. The comparison inverted (`==` for `!=`, so a *match* traps and a mismatch returns).
//    Failed 1 of 200 on the same property from the other side -- the surviving paths are
//    exactly the mismatched ones.
//
// 3. The return off by one (`(void*)b` instead of `(void*)(b + 1)` -- handing back the
//    header rather than the payload). Failed 2 of 194 on "what comes back is the box's
//    payload slot" and "the payload read through the returned pointer is the marker".
//
// ---- SCOPE: what this model does not cover ----
//
// 1. THE TRAP ITSELF IS NOT OBSERVED: a trap is a cut path, so the model proves no
//    mismatch *returns*, not what the trap prints or exits with. The tinyunit suite
//    (`any_test.c`, box_expect_traps_on_a_mismatched_tag) observes the real exit status.
//
// 2. TRAPPING TOO MUCH IS INVISIBLE HERE. A mutation that traps on every pair cuts every
//    path and passes this model vacuously. It is not a silent hole in practice -- the
//    matching-tag tinyunit test and every erased-recovery corpus program
//    (types/list_literal_erased_into_any_recovered.neon among them) fail loudly on a
//    runtime that traps a matching cast -- but it is not this proof.
//
// 3. THE PAYLOAD IS A SCALAR and the witness has no retain/release: ownership across the
//    boxing boundary is the round-trip model's job (rule 7 is satisfied there, where the
//    property is about ownership; here the property is about the tag gate).

#include "../support/cbmc_support.h"
#include "libneon_rt.h"

#include <stdio.h>

// Rule 4. The mismatch path reaches `neon_trap`, whose `fprintf`/`fflush` would pull a
// `FILE` model into every trap site. The model has nothing to say about stdio.
int fprintf(FILE* stream, const char* fmt, ...) { (void)stream; (void)fmt; return 0; }
int fflush(FILE* stream) { (void)stream; return 0; }

uint64_t nondet_u64(void);

// A scalar witness: eight bytes, nothing counted. See SCOPE 3.
static const neon_witness I64_W = { sizeof(int64_t), 0, 0, 0, 0 };

int main(void) {
    uint64_t stamped = nondet_u64();
    uint64_t asked = nondet_u64();

    int64_t marker = 424242;
    neon_value v = neon_box_new(&marker, &I64_W, stamped);

    // Traps -- cutting the path -- on `asked != stamped`. Every state that reaches the
    // next line is one the gate let through.
    int64_t* p = (int64_t*)neon_box_expect(v, asked);

    PROVE(asked == stamped,
          "a returning neon_box_expect means the asked tag is the tag the box was "
          "stamped with -- no mismatched cast escapes the trap");
    PROVE(p == (int64_t*)neon_box_payload(v), "what comes back is the box's payload slot");
    PROVE(*p == 424242, "the payload read through the returned pointer is the marker");

    // Balance the allocation for the default memory-leak check. The scalar witness has no
    // `release`, so this is the box's own free and nothing else -- ownership behaviour is
    // the round-trip model's property.
    neon_release((neon_header*)v);
    return 0;
}
