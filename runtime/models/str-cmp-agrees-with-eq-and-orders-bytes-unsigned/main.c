// Model: `neon_str_eq` and `neon_str_cmp` over two short strings of unconstrained bytes and
// unconstrained lengths.
//
// THE INVARIANT: `neon_str_eq(a, b)` is true exactly when `neon_str_cmp(a, b)` is 0; `cmp`
// is the byte-lexicographic order with bytes compared as UNSIGNED and a shorter string
// sorting before a longer one it is a prefix of; and `cmp` is antisymmetric while `eq` is
// symmetric.
//
// `eq` and `cmp` are two hand-written implementations of one relation, and nothing in
// `string.c` forces them to agree: `eq` short-circuits on length and then walks bytes,
// while `cmp` walks to the first differing byte and falls back to a length tiebreak. A
// divergence is a program where `a == b` is true and sorting says otherwise. Two subtleties
// carry real risk. First, both cast each byte to `unsigned char` before comparing, because
// plain `char` is signed on this target -- a byte over 127 read as signed is negative and
// would sort *before* an ASCII byte, disagreeing with `memcmp` and mis-ordering UTF-8; the
// bytes here are fully unconstrained, so that case is reached. Second, `cmp`'s three-way
// result is built from the first differing byte and only then from the length tiebreak,
// which `eq` has no counterpart to.
//
// Every string here is <= NEON_STR_SHORT bytes, which is the short-string fast path: `eq`
// and `cmp` both run their own byte loop rather than calling `memcmp`. That is deliberate on
// two counts -- it is the path a map key, an interpolation fragment and a formatted integer
// all take (the profile in `core.h` that set the boundary), and it keeps the model off
// CBMC's `memcmp`, which is imprecise with a symbolic byte count (README rule 4) and would
// fail every downstream property spuriously. The `memcmp` path is SCOPE 1.
//
// The bytes and both lengths are unconstrained (up to the short-path bound), so within that
// bound this is exhaustive rather than sampled: every pair of lengths including two empties,
// and every byte value including the > 127 ones the unsigned cast exists for.
//
// Verifies `src/string.c` compiled from source; see rule 1.
//
// ---- ONE CHECK IS OFF ----
//
// `checks-off.txt` drops `--conversion-check` for this model, and only this model. The
// shipped `(unsigned char)pa[i]` cast is a well-defined narrowing that the check flags as
// lossy for any byte over 127 -- exactly the bytes the unsigned comparison exists to handle
// and this model exists to reach. The check is the wrong oracle for that cast, not a defect
// it catches, and CBMC has no `--no-conversion-check` to scope it out at the command line.
// Every other default check stays on; this is a documented hole, per rule 5.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Baseline: 106 properties, VERIFICATION SUCCESSFUL. Four mutations, each reverted after.
//
// 1. `neon_str_cmp` comparing bytes as SIGNED `char` (the `(unsigned char)` casts dropped).
//    Failed 1 of 106 on "cmp is the unsigned byte-lexicographic order" -- a byte over 127
//    reads as negative and sorts before an ASCII byte, the exact bug the casts prevent and
//    the reason `--conversion-check` is off (it would otherwise flag the correct casts
//    before this mutation could be reached). This is the model's central claim.
//
// 2. `neon_str_cmp` dropping its length tiebreak (`return 0` in place of the shorter-first
//    rule). Failed 2 of 106 on the order claim and on "eq is true exactly when cmp is 0" --
//    a proper prefix now compares equal to the longer string it prefixes.
//
// 3. `neon_str_eq` dropping its early length check, so it walks a common prefix and calls a
//    shorter string equal to a longer one. Failed 3 of 106 on "eq agrees with that
//    relation", "eq is true exactly when cmp is 0" and "eq is symmetric".
//
// 4. `neon_str_cmp` inverting the byte comparison's sign (`< ? 1 : -1`). Failed 1 of 106 on
//    the order claim; antisymmetry alone would not have caught it (negating both operands'
//    order keeps `cmp(b,a) == -cmp(a,b)`), which is why the model checks `cmp` against an
//    independent reference and not only its own algebraic laws.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. THE memcmp PATH. Strings longer than NEON_STR_SHORT take `eq`/`cmp`'s `memcmp` branch,
//    which this model does not reach. CBMC's `memcmp` leaves the copied/compared bytes
//    unconstrained under a symbolic count and fails downstream properties spuriously
//    (README rule 4), so driving it needs concrete-length strings and is a separate model.
//    The two branches share their length logic but not their inner compare, so this proves
//    the short one only.
//
// 2. LENGTHS REACH THREE. Three is the least that walks the compare loop past its first
//    step while staying well under `--unwind`; every length from 0 to 3 is covered,
//    including both-empty. A length that crosses the short/long boundary is SCOPE 1.
//
// 3. LITERALS, SO OWNERSHIP IS NOT EXERCISED. Both operands are `neon_str_lit` (owner
//    NULL), which is right for a comparison -- `eq` and `cmp` borrow and touch no count --
//    but it means the "borrows, never consumes" claim the list model makes is trivial here
//    and not asserted: there is no counted owner to release. The borrow contract for a
//    heap-backed operand is the list model's `elem` witness, not this.
//
// 4. TRANSITIVITY AND TOTALITY are not proved: antisymmetry across a pair is checked, but
//    the order law across three strings is not. `cmp` is only as ordered as byte comparison
//    is, which for bytes is total by construction.

#include "../support/cbmc_support.h"
#include "libneon_rt.h"

#include <stdio.h>

// Rule 4. Unused `string.c` neighbours (`concat`, `repeat`, ...) reach `neon_trap`, whose
// `fflush`/`fprintf` pull a `FILE` in; `--drop-unused-functions` removes those neighbours,
// but the stubs cost nothing and keep the model robust if one becomes reachable.
int fprintf(FILE* stream, const char* fmt, ...) { (void)stream; (void)fmt; return 0; }
int fflush(FILE* stream) { (void)stream; return 0; }

#define MAXLEN 3 // <= NEON_STR_SHORT (6): every string stays on the byte-loop fast path

static char abuf[MAXLEN];
static char bbuf[MAXLEN];

// The reference relation, written independently of the code under test: byte-lexicographic
// with bytes taken as `unsigned char`, shorter-is-smaller on a tie. Normalised to -1/0/1.
static int ref_cmp(size_t la, size_t lb) {
    size_t n = la < lb ? la : lb;
    for (size_t i = 0; i < MAXLEN; i++) { // constant bound, rule 3
        if (i >= n) break;
        unsigned char ca = (unsigned char)abuf[i];
        unsigned char cb = (unsigned char)bbuf[i];
        if (ca != cb) return ca < cb ? -1 : 1;
    }
    return la < lb ? -1 : (la > lb ? 1 : 0);
}

int main(void) {
    for (size_t i = 0; i < MAXLEN; i++) { // constant bound, rule 3
        abuf[i] = (char)nondet_uint(); // full 0..255 byte range, so the > 127 case is reached
        bbuf[i] = (char)nondet_uint();
    }
    size_t la = NONDET_UPTO(MAXLEN, "left length; every length 0..3 on the short path");
    size_t lb = NONDET_UPTO(MAXLEN, "right length; n != m reaches the length tiebreak");

    neon_str a = neon_str_lit(abuf, la);
    neon_str b = neon_str_lit(bbuf, lb);

    bool eq = neon_str_eq(a, b);
    int c = neon_str_cmp(a, b);
    int rc = ref_cmp(la, lb);
    bool req = (rc == 0); // equal iff the byte-lexicographic comparison ties, lengths and all

    PROVE(c == rc, "cmp is the unsigned byte-lexicographic order with a shorter-first tiebreak");
    PROVE(eq == req, "eq agrees with that relation");
    PROVE(eq == (c == 0), "eq is true exactly when cmp is 0");
    PROVE(neon_str_cmp(b, a) == -c, "cmp is antisymmetric");
    PROVE(neon_str_eq(b, a) == eq, "eq is symmetric");
    return 0;
}
