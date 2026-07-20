// Model: concatenates every pair of lists with lengths 0..2 and reads the result back
// element by element through the public accessor.
//
// THE INVARIANT: `neon_list_concat`'s result has length exactly `a->len + b->len`, holds
// `a`'s elements then `b`'s in that order across the seam, and leaves each element owned
// exactly once -- the result owns it, and both operands have been consumed.
//
// The seam is where this goes wrong. `concat` writes `b` at `r->data + a->len * sz`, a
// second offset computation independent of the first `memcpy`, and then retains over the
// *combined* range `0 .. r->len`. Three separate uses of `sz` and `len` that must agree;
// disagree on any one and elements land at the wrong offset or the retain loop covers
// the wrong span, leaking one end and over-releasing the other. Identities here are
// handed out as `0..n-1` on the left and `n..n+m-1` on the right, so the result must read
// back as the single ascending run `0..n+m-1` -- an off-by-one at the seam shows up as a
// gap or a repeat, not as a plausible-looking list.
//
// The zero-length pairs are the ones an ordinary program reaches and a test rarely
// writes: `xs ++ []`. An empty list has `data == NULL`, and `memcpy` requires valid
// pointers even for a count of zero (C17 7.24.1p2) -- it also carries `nonnull`, from
// which GCC and Clang are entitled to infer the arguments are non-NULL and delete later
// checks, so this is exploitable UB rather than a technicality. `concat` of two empty
// lists additionally forms `NULL + 0`, which is UB in its own right. `list.c` guards both
// copies with `if (a->len != 0)` / `if (b->len != 0)`; that guard was added because this
// model's predecessor found its absence, and the `(0, 0)`, `(0, m)` and `(n, 0)` arms
// below are what keep it there.
//
// Ownership is observable only because the element is refcounted with a real `release`
// (rule 7); with a scalar the retain loop is dead code and every one of the bugs above is
// invisible. The 16-byte width, with a `tag` derived from the `id`, turns a slot-width
// bug into a payload at the wrong offset rather than a coincidentally-correct read.
//
// Verifies `src/list.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Baseline: 1187 properties, VERIFICATION SUCCESSFUL. Four mutations, each reverted.
//
// 1. The two copies' destinations swapped, so the result is `b ++ a` while its length and
//    ownership stay right. Failed 2 of 1187, on "element k of the concatenation is the
//    k'th of a then b" and "its bytes crossed the seam intact". Nothing about refcounts
//    fires -- the accounting is still balanced -- which is the point of asserting order
//    separately from ownership.
//
// 2. The retain loop deleted, so the result shares element references it does not own.
//    Failed 3 of 1107, on "concat leaves exactly one owned reference per element",
//    "dropping the concatenation releases every element exactly once", and in the witness
//    on "no element is released more times than it was retained" -- the over-release
//    oracle fires at the offending `release` call, not at some later use-after-free.
//    Shipped: every `a ++ b` on a list of refcounted elements is a use-after-free once
//    either operand is dropped.
//
// 3. The retain loop's bound mistyped `a->len` for `r->len`, so only the left operand's
//    elements are retained. The plausible typo, and a strictly subtler form of 2. Caught
//    identically, 3 of 1187 on the same claims.
//
// 4. REGRESSION CHECK -- the defect the previous generation of this model found, restored:
//    the `if (a->len != 0)` / `if (b->len != 0)` guards at list.c:151 and :154 removed, so
//    `memcpy` is called with a NULL pointer and a count of zero. Failed 25 of 1187, all
//    inside CBMC's `memcpy` contract: "pointer relation: pointer NULL in b->data" and
//    kin. The fix stays fixed and this model still catches it going. `memcpy(NULL, NULL,
//    0)` is UB the compiler may act on -- `nonnull` lets it delete a later null check --
//    not a technicality, and concatenating two empty lists additionally forms `NULL + 0`.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. Lengths are enumerated concretely up to 2 per side, nine pairs, because `concat`
//    sizes a `memcpy` and a `malloc` by `len * sz` and CBMC's built-in `memcpy` is
//    imprecise when the byte count is symbolic -- it leaves the copied bytes
//    unconstrained and every property below fails spuriously (shape requirement 2; not
//    Neon-specific, it reproduces in twenty lines of plain C). The enumeration is the
//    full cross product, so no pair within the bound is skipped, but coverage is BY
//    ENUMERATION: raising the bound means adding arms, and lengths above 2 are NOT
//    proved. 2 is the bound because what `concat`'s code distinguishes is
//    empty / one / more-than-one on each side; its loops are otherwise length-generic.
//
// 2. ALIASING IS NOT COVERED. The two operands are always distinct lists. `concat(a, a)`
//    would release the same list twice and is not emitted by codegen, so NOT proved:
//    anything about a self-concatenation.
//
// 3. The result's `cap` is exactly `a->len + b->len`, so the result is full and a
//    subsequent push would grow it. That push is not performed. NOT proved: that a
//    concatenation result grows correctly.
//
// 4. No length here reaches a `cap` where `a->len + b->len` or `ncap * sz` could
//    approach `size_t` overflow, so the overflow checks say nothing about huge lists;
//    one cannot be built inside a model.

#include "../support/cbmc_support.h"

#include <stdio.h>

#include "libneon_rt.h"

// Rule 4. `neon_trap` calls `fflush`/`fprintf`, and CBMC's models of those pull a `FILE`
// and its buffers in at *every* trap site -- reachable here from every allocation check
// and every OOM branch `--malloc-fail-null` opens. Left alone they account for most of
// the program's addressed objects and put it over CBMC's default `--object-bits 8`,
// which the shared CMake target does not override. What a trap prints is not a property
// of the list.
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
#define CAT_MAX 2
#define MAXSLOTS (2 * CAT_MAX)
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
    // often, not at some later use.
    PROVE(live[e->id] > 0, "no element is released more times than it was retained");
    live[e->id]--;
}

static bool elem_eq(const void* a, const void* b) {
    return ((const elem*)a)->id == ((const elem*)b)->id;
}

static int elem_cmp(const void* a, const void* b) {
    uint64_t x = ((const elem*)a)->id, y = ((const elem*)b)->id;
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

// Build a fresh list holding identities `first .. first + n - 1`. `n` is concrete at
// every call site (shape requirement 2), so it is also the loop's literal trip count.
static neon_list* build_run(unsigned first, unsigned n) {
    neon_list* l = neon_list_new(&ELEM_W);
    for (unsigned i = 0; i < n; i++) { // constant bound, rule 3: n is a literal here
        l = push_owned(l, first + i);
    }
    return l;
}

// `n` and `m` arrive concrete from the enumeration in `main`; see SCOPE note 1.
static void concat_pair(unsigned n, unsigned m) {
    neon_list* a = build_run(0, n);
    neon_list* b = build_run(n, m);

    neon_list* r = neon_list_concat(a, b); // consumes both

    PROVE(r->len == (size_t)n + m, "concat's length is the exact sum of the two");
    PROVE(r->len <= r->cap, "concat maintains len <= cap");

    // Identities were handed out 0..n-1 then n..n+m-1, so the concatenation must read
    // back as the single run 0..n+m-1 -- order preserved across the seam, with no gap
    // and no repeat where `b`'s copy begins.
    for (unsigned k = 0; k < MAXSLOTS; k++) { // constant bound, rule 3
        if (k >= n + m) break;
        elem* s = (elem*)neon_list_at(r, (int64_t)k);
        PROVE(s == (elem*)(r->data + (size_t)k * ELEM_W.size),
              "at(k) addresses data + k * w->size in the concatenation");
        PROVE(s->id == k, "element k of the concatenation is the k'th of a then b");
        PROVE(s->tag == ELEM_TAG(k), "its bytes crossed the seam intact");
        PROVE(live[k] == 1, "concat leaves exactly one owned reference per element");
    }

    neon_release((neon_header*)r);
    for (unsigned k = 0; k < NIDS; k++) { // constant bound, rule 3
        PROVE(live[k] == 0,
              "dropping the concatenation releases every element exactly once");
    }
}

int main(void) {
    // Exhaustive enumeration of the concrete-length pairs. Each `case` is a distinct pair
    // of literals, so the `memcpy` byte counts and the `malloc` size inside `concat` are
    // constants; the set of cases is the full cross product of 0..CAT_MAX on both sides,
    // so no pair within the bound is skipped. See SCOPE note 1 for why this is necessary
    // and what it costs.
    unsigned k = NONDET_UPTO((CAT_MAX + 1) * (CAT_MAX + 1) - 1,
        "selects one of the nine (n, m) length pairs below; harness dispatch only, it "
        "constrains no input the runtime sees");
    switch (k) {
        case 0: concat_pair(0, 0); break;  // both empty: data == NULL on both sides
        case 1: concat_pair(0, 1); break;  // empty left: NULL source for the first copy
        case 2: concat_pair(0, 2); break;
        case 3: concat_pair(1, 0); break;  // empty right: NULL source for the second copy
        case 4: concat_pair(1, 1); break;
        case 5: concat_pair(1, 2); break;
        case 6: concat_pair(2, 0); break;
        case 7: concat_pair(2, 1); break;
        default: concat_pair(2, 2); break; // more than one on each side of the seam
    }
    return 0;
}
