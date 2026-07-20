// Model: compares two lists over a common prefix, at every pair of lengths up to 3, with
// both `eq` and `cmp`.
//
// THE INVARIANT: `neon_list_eq(a, b)` is true exactly when `neon_list_cmp(a, b)` is 0,
// `cmp` is antisymmetric, a proper prefix sorts before the longer list, and both borrow
// their operands rather than consuming them.
//
// `eq` and `cmp` are two separate implementations of one relation. `cmp` walks to the
// first differing element and falls back to a length tiebreak; `eq` short-circuits on
// length and then walks with `w->eq`, deliberately *not* defined as `cmp(a, b) == 0`
// because a length check rejects most unequal pairs without touching an element. That
// duplication is the risk: nothing in `list.c` forces the two to agree, and a divergence
// is a program where `a == b` is true and sorting says otherwise. The lists here share a
// common prefix by construction -- both hold identities `0..`, so no element ever
// differs -- which puts the whole weight on `cmp`'s final `a->len < b->len ? -1 : ...`
// tiebreak, the line that has no counterpart in `eq` at all.
//
// Borrowing is the other half and it is an ownership claim, not a comparison one. `eq`
// and `cmp` are reached through operators whose operands the refcount pass releases
// itself, so a `release` inside either would double-free; both take `const neon_list*`
// and neither touches `rc`, and the `rc == 1` assertions after the calls are what pin
// that. This is only observable because the element is refcounted with a real
// `retain`/`release` (rule 7) -- with a scalar, a stray release inside the comparison
// loop is a no-op and the model proves nothing. The 16-byte element width, with a `tag`
// derived from the `id`, additionally makes the comparison loops' own slot arithmetic
// checkable: `elem_cmp` and `elem_eq` assert nothing, but a wrong stride would hand them
// a torn element, and every element they see is read back intact by construction of the
// identities.
//
// Both zero-length cases are reached, including comparing two empty lists, where each
// loop body runs zero times and the answer comes entirely from the length tiebreak.
//
// Verifies `src/list.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// This model FOUND A BLIND SPOT IN ITSELF and was strengthened; the record is worth
// reading in order.
//
// Baseline as originally written: 1070 properties, VERIFICATION SUCCESSFUL.
//
// 1. `neon_list_cmp`'s length tiebreak replaced with `return 0`, so two lists of different
//    length over a common prefix compare equal. Failed 2 of 1046, on "eq is true exactly
//    when cmp is 0" and "a proper prefix sorts before the longer list". Caught.
//
// 2. `neon_list_eq`'s early `a->len != b->len` return deleted, so equality walks a prefix
//    and calls a shorter list equal to a longer one. Failed 24 of 1058 -- the walk also
//    runs off the shorter buffer, so most of those are bounds failures rather than the
//    logical claim. Caught.
//
// 3. `neon_list_cmp` negating the element comparator's answer: `return c` -> `return -c`
//    at the first differing element. THE ORIGINAL MODEL PASSED THIS, 0 of 1071. It was
//    blind for a stated reason -- SCOPE note 1 said both lists held identities `0..`, so
//    one was always a prefix of the other and `elem_cmp` never once returned non-zero.
//    Every claim about `cmp` rested on the length tiebreak. A declared gap is still a gap:
//    shipped, this reverses the sort order of every list in the language, and it is
//    invisible to `==` because negating a non-zero value leaves it non-zero -- so the
//    model's central `eq == (c == 0)` claim cannot see it by construction.
//
//    FIXED, in `$MAIN`, by giving `build_run` a nondet index `d` at which the right list
//    holds a strictly greater identity (`d == CMP_MAX` keeps the old pure-prefix case, so
//    nothing that was covered was given up), and adding two claims: "cmp carries the sign
//    the first differing element gives it, and falls back to the length tiebreak only when
//    no element differs", and "the first differing element decides cmp, not the lengths
//    and not a later element". New baseline 1074 properties, VERIFICATION SUCCESSFUL.
//
//    Re-run against the strengthened model, mutation 3 fails 2 of 1075 on exactly those
//    two new claims. Mutation 1 still fails, 2 of 1050. And a third, previously
//    unreachable, mutation now bites: `cmp`'s element loop bound forced to zero so it
//    consults only lengths -- 3 of 1073, on both new claims plus "eq is true exactly when
//    cmp is 0".
//
// All mutations reverted; the strengthening is not a mutation and is kept.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. ONE DIFFERING ELEMENT, ALWAYS IN THE SAME DIRECTION. The right list differs from the
//    left at a single nondet index and is always the *greater* there, so `cmp` is only
//    ever driven to -1 by an element. NOT proved: that two differing elements are decided
//    by the earlier one (only one ever differs), or that a *smaller* right element yields
//    +1 by the element loop rather than by chance -- antisymmetry gets partway there but
//    the +1 case is inferred from it, not driven directly.
//
// 2. THE ORDER LAW IS NOT PROVED. Antisymmetry is checked here; transitivity across three
//    lists is not, and neither is totality. Both are properties of the element `cmp`,
//    which codegen supplies -- `neon_list_cmp` can only be as ordered as its witness.
//
// 3. ALIASING IS NOT COVERED. The two operands are always distinct lists; `cmp(a, a)` is
//    not driven.
//
// 4. Lengths reach 3 and growth never runs, so NOT proved: that the comparison loops
//    address correctly into a buffer that has been `realloc`ed.
//
// 5. `w->cmp` is assumed non-NULL, as `neon_list_cmp` calls it unconditionally. NOT
//    proved: anything about a witness without a `cmp`, which is what an unordered
//    element type would have.

#include "../support/cbmc_support.h"

#include <stdio.h>

#include "libneon_rt.h"

// Rule 4. `neon_trap` calls `fflush`/`fprintf`, and CBMC's models of those pull a `FILE`
// and its buffers in at *every* trap site -- reachable here from every allocation check
// building the two lists and every OOM branch `--malloc-fail-null` opens. Left alone they
// account for most of the program's addressed objects and put it over CBMC's default
// `--object-bits 8`, which the shared CMake target does not override. What a trap prints
// is not a property of the list.
int fprintf(FILE* stream, const char* fmt, ...) { (void)stream; (void)fmt; return 0; }
int fflush(FILE* stream) { (void)stream; return 0; }

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
#define CMP_MAX 3
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
    // The over-release oracle: it fails at the call in list.c that released once too
    // often, not at some later use. A comparison that released an operand's elements
    // fails here, at that call.
    PROVE(live[e->id] > 0, "no element is released more times than it was retained");
    live[e->id]--;
}

// Both comparison callbacks assert the bytes they are handed are intact, so a wrong
// stride inside the comparison loops fails here rather than producing a wrong answer.
static bool elem_eq(const void* a, const void* b) {
    const elem* x = (const elem*)a;
    const elem* y = (const elem*)b;
    PROVE(x->tag == ELEM_TAG(x->id) && y->tag == ELEM_TAG(y->id),
          "eq's walk hands the witness two whole elements, so its stride is w->size");
    return x->id == y->id;
}

static int elem_cmp(const void* a, const void* b) {
    const elem* p = (const elem*)a;
    const elem* q = (const elem*)b;
    PROVE(p->tag == ELEM_TAG(p->id) && q->tag == ELEM_TAG(q->id),
          "cmp's walk hands the witness two whole elements, so its stride is w->size");
    uint64_t x = p->id, y = q->id;
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

// The identity a run holds at index `i`. A run is `0, 1, 2, ...` except at index `d`,
// where it holds a strictly larger identity -- `d == CMP_MAX` means "no bump", i.e. the
// plain prefix run. Bumping by `BUMP` keeps every identity under `NIDS` and keeps the
// bumped one strictly greater than the unbumped one at the same index, which is what makes
// the expected sign of `cmp` a closed form below.
#define BUMP 4
static uint64_t id_at(unsigned i, unsigned d) { return i == d ? (uint64_t)i + BUMP : (uint64_t)i; }

// Build a fresh list holding `id_at(0, d) .. id_at(n - 1, d)`.
static neon_list* build_run(unsigned n, unsigned d) {
    neon_list* l = neon_list_new(&ELEM_W);
    for (unsigned i = 0; i < CMP_MAX; i++) { // constant bound, rule 3
        if (i >= n) break;
        l = push_owned(l, id_at(i, d));
    }
    return l;
}

int main(void) {
    unsigned n = NONDET_UPTO(CMP_MAX,
        "left length; 2 is the least that walks the comparison loop more than once, and "
        "every length up to the bound is explored, including 0");
    unsigned m = NONDET_UPTO(CMP_MAX,
        "right length; same bound. n != m is what reaches the prefix case where cmp's "
        "length tiebreak alone decides");

    unsigned d = NONDET_UPTO(CMP_MAX,
        "the index at which the right list's element differs, held strictly greater than "
        "the left's. CMP_MAX means no element differs, which is the pure-prefix case where "
        "cmp's length tiebreak alone decides; every smaller value puts a differing element "
        "at that index, so the element loop must return before reaching the tiebreak");

    // `a` is the plain run `0 .. n-1`; `b` is the same except at index `d`, where it holds
    // a strictly larger identity. When `d` lands inside both lists the answer must come
    // from the element loop and carry the element comparator's sign; when it does not, the
    // two are a prefix pair and the length tiebreak decides.
    neon_list* a = build_run(n, CMP_MAX);
    neon_list* b = build_run(m, d);

    bool eq = neon_list_eq(a, b);
    int c = neon_list_cmp(a, b);

    unsigned p = n < m ? n : m;               // the common prefix the element loop walks
    bool differs = d < p;                     // ... and whether a differing element is in it
    // If an element differs it is at index `d` and b's is the greater, so `cmp` must be -1
    // and must have returned there -- before, and regardless of, the length tiebreak.
    int expect_c = differs ? -1 : (n < m ? -1 : (n > m ? 1 : 0));
    bool expect_eq = (n == m) && d >= n;

    PROVE(eq == expect_eq, "lists are equal iff they are the same length and no element differs");
    PROVE(eq == (c == 0), "eq is true exactly when cmp is 0");
    PROVE(c == expect_c,
          "cmp carries the sign the first differing element gives it, and falls back to "
          "the length tiebreak only when no element differs");
    PROVE(!differs || c == -1,
          "the first differing element decides cmp, not the lengths and not a later element");
    PROVE(neon_list_cmp(b, a) == -c, "cmp is antisymmetric on these lists");
    PROVE(neon_list_eq(b, a) == eq, "eq is symmetric");

    PROVE(a->header.rc == 1 && b->header.rc == 1, "cmp and eq borrow, never consume");
    PROVE(a->len == n && b->len == m, "and leave both operands' lengths alone");
    for (unsigned k = 0; k < NIDS; k++) { // constant bound, rule 3
        int expect = 0;
        for (unsigned i = 0; i < CMP_MAX; i++) { // constant bound, rule 3
            if (i < n && id_at(i, CMP_MAX) == (uint64_t)k) expect++;
            if (i < m && id_at(i, d) == (uint64_t)k) expect++;
        }
        PROVE(live[k] == expect,
              "comparison releases no element of either operand");
    }

    neon_release((neon_header*)a);
    neon_release((neon_header*)b);
    for (unsigned k = 0; k < NIDS; k++) { // constant bound, rule 3
        PROVE(live[k] == 0, "dropping both lists releases every element exactly once");
    }
    return 0;
}
