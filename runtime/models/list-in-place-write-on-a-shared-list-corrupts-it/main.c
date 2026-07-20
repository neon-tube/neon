// Model: `neon_list_set_scalar_inplace` called on a list with `rc > 1` -- the caller's
// precondition, rendered as a live proof obligation.
//
// THE INVARIANT (stated in the NEGATIVE, deliberately): when `neon_list_set_scalar_inplace`
// is called on a list that a second holder also references, the write goes straight through
// to that second holder. No copy is taken, the two references remain the same object, and
// the other holder observes bytes it never wrote.
//
// ---- READ THIS BEFORE CONCLUDING ANYTHING ABOUT THE RUNTIME ----
//
// This model documents a PRECONDITION OF THE CALLER. It is NOT a bug report, it is NOT
// asserting desirable behaviour, and the behaviour it pins down is NOT a defect in
// `src/list.c`.
//
// `neon_list_set_scalar_inplace` is specified (list.c:114-117) to be called only on a list
// the caller has ALREADY established is sole-owned. It has no `rc` test and takes no copy,
// on purpose: that absence is the entire performance argument for the function existing
// separately from `neon_list_set_scalar`, which does check. Establishing sole ownership is
// the *optimiser's* job -- `ir::unique` proves the list is sole-owned before a write loop,
// emits one `neon_list_ensure_unique` ahead of the loop, and only then rewrites the writes
// inside it to this function. The runtime is entitled to assume that has happened.
//
// So what is asserted below is what a violated precondition looks like, and it is asserted
// so that the violation cannot become quiet. Two futures make this model fail, and each of
// them is a real event somebody needs to be told about:
//
//   * Somebody "fixes" `set_scalar_inplace` to test `rc` and copy. That is not a fix -- it
//     silently reintroduces the per-write refcount test and the returned-pointer aliasing
//     barrier that `ir::unique` exists to eliminate, and it would do so without any
//     benchmark obviously regressing enough to notice. If that is a deliberate decision,
//     this model is the place it gets recorded; if it is not, this model catches it.
//
//   * `ir::unique` stops establishing sole ownership -- the gate is widened, the
//     `ensure_unique` call is sunk into a branch that does not dominate the loop, or the
//     rewrite fires on a list that escapes. Then real programs take this path, and the
//     consequence is exactly what is asserted here: a list somebody else is holding mutates
//     underneath them, silently, with no output difference until the other holder is read.
//     There is no model on the runtime side that can catch that, because from the runtime's
//     view the call is well-formed. This model at least makes the failure mode *named* and
//     *proved*, so that the incident report has something to point at.
//
// The corresponding POSITIVE property -- that the copy-on-write cousin does take a copy
// when shared -- is `list-scalar-write-still-copies-when-shared`. Read the two together:
// the pair is what says the split between the two functions is the one that was intended.
//
// Verifies `src/list.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// The mutation this model exists for is the first future named above: somebody "fixes"
// `neon_list_set_scalar_inplace` by giving it the `rc` test its copy-on-write cousin has.
// Adding `if (l->header.rc > 1) l = neon_list_ensure_unique(l);` ahead of the memcpy was
// confirmed to fail (9 of 830, baseline 836 properties under the mutation), and reverted.
//
// It failed on the two claims that carry the model's meaning -- "the second holder observes
// the write: this is the precondition's consequence, not desirable behaviour" and "and
// observes all sz bytes of it, so the corruption is a whole element rather than a torn one"
// -- and on "and neither reference was consumed: the in-place write does not participate in
// refcounting at all", which is the claim that pins down *why* the fix is not a fix.
// `ensure_unique` consumes the reference it was handed, so the harness's second holder is
// left dangling: CBMC also reported a use-after-free cascade through `neon_release` and a
// leaked allocation. That is the real shape of the "fix" -- it does not merely cost
// performance, it changes the function's ownership contract, and every `ir::unique` call
// site was written against the contract that it does not.
//
// So this model is live, not decorative. The negative statement is doing work: it is the
// only thing in the tree that would turn that plausible, well-intentioned edit into a red
// build rather than a silent 14.7% regression on the brainfuck loop.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. THE ELEMENT TYPE IS A SCALAR, AND THAT IS THE CORRECT DOMAIN -- a deliberate,
//    reasoned departure from rule 7, not an oversight, and it must not be "fixed".
//    `neon_list_set_scalar_inplace` carries the documented precondition (list.c:103-104)
//    that the element type is NOT refcounted: it overwrites the slot with no release, so
//    calling it for a counted element leaks the value being overwritten. A refcounted
//    element is therefore outside the function's contract, and a model built on one would
//    assert properties of a call the runtime forbids. The witness here accordingly has
//    `retain` and `release` NULL, exactly as codegen emits for an `i64` or a `bool`.
//
//    Rule 7's actual demand -- exercise the case that makes the bug visible -- is met by
//    the *shape* of the element instead of by its ownership: 16 bytes, deliberately not a
//    machine word, with `tag` derived from `id`, so a write landing at the wrong offset or
//    covering the wrong width is visible as a torn or displaced payload. An `int64_t`
//    element would hide exactly that.
//
// 2. THIS MODEL SAYS NOTHING ABOUT `ir::unique`. It proves what the runtime does when the
//    precondition is violated; it cannot prove that the precondition holds at any real call
//    site, because `ir::unique` is a Rust pass in the compiler and no CBMC model of the
//    runtime can see it. The obligation that every rewritten write is dominated by an
//    `ensure_unique` on the same list, with no intervening escape, belongs in the compiler
//    and needs a check there.
//
// 3. `rc` IS EXACTLY 2 AND THE LENGTH IS CONCRETELY 2. Higher counts and longer lists reach
//    no different branch -- `set_scalar_inplace` reads neither `rc` nor any slot but the
//    one it writes. Two of each is the least that gives a second holder and a neighbouring
//    slot to check.
//
// 4. Out-of-range indices are the other model's subject; this one enters in range.
//
// 5. Out-of-memory is not a recoverable path in this runtime -- every allocation failure
//    reaches `neon_trap`, which `_exit`s. CBMC does take those branches under
//    `--malloc-fail-null` and proves nothing is dereferenced before the trap, but a leak
//    check cannot fire past a trap, so "no leak on OOM" is vacuous by design rather than
//    proved.

#include "../support/cbmc_support.h"

#include <stdio.h>

#include "libneon_rt.h"

// Rule 4. `neon_trap` calls `fflush`/`fprintf`, every allocation check in list.c can reach a
// trap, and CBMC's models of those pull a `FILE` and its buffers into each of those sites.
// What a trap prints is not a property of the list, and the trap still terminates the path
// via `_exit`.
int fprintf(FILE* stream, const char* fmt, ...) { (void)stream; (void)fmt; return 0; }
int fflush(FILE* stream) { (void)stream; return 0; }

// ---- the element type and its witness ----
//
// 16 bytes with a self-checking tag, `retain`/`release` NULL. See SCOPE note 1: the scalar
// element is the function's documented domain, and the reasoning is on the page so that a
// later reader does not "correct" it into a refcounted one.
typedef struct {
    uint64_t id;
    uint64_t tag;
} elem;

#define ELEM_TAG(i) (0xA51A51A500000000ULL ^ (uint64_t)(i))
#define ELEM_SZ sizeof(elem)

static const neon_witness ELEM_W = {
    .size = sizeof(elem),
    .retain = NULL,
    .release = NULL,
    .eq = NULL,
    .cmp = NULL,
};

static elem staging;

#define N 2
#define FRESH 7

static neon_list* build(void) {
    neon_list* l = neon_list_new(&ELEM_W);
    for (unsigned i = 0; i < N; i++) { // constant bound, rule 3
        staging.id = i;
        staging.tag = ELEM_TAG(i);
        l = neon_list_push(l, &staging);
    }
    return l;
}

int main(void) {
    neon_list* l = build();
    PROVE(l->len == N, "the fixture holds exactly two elements");

    // A second holder, exactly as a program that bound the list to another name would have.
    // From here the precondition of `neon_list_set_scalar_inplace` is FALSE, and everything
    // below describes the consequence -- not a defect. See the header.
    neon_retain((neon_header*)l);
    neon_list* keep = l;
    char* keep_data = l->data;
    size_t keep_len = l->len;
    PROVE(l->header.rc == 2, "the list now has two holders: the precondition is violated");

    unsigned i = NONDET_UPTO(N - 1, "the slot written; both slots of the fixture are "
                                    "explored, so the write-through is proved for the "
                                    "written slot and the untouched neighbour alike");

    staging.id = FRESH;
    staging.tag = ELEM_TAG(FRESH);
    neon_list_set_scalar_inplace(l, (int64_t)i, &staging, ELEM_SZ);

    // ---- no copy was taken ----
    PROVE(l == keep,
          "with rc > 1 the in-place write still mutates the one shared object: it has no "
          "rc test and cannot copy, which is the whole point of it existing");
    PROVE(keep->data == keep_data,
          "the other holder's buffer was neither replaced nor reallocated -- it is the "
          "very buffer that was written into");
    PROVE(keep->len == keep_len, "the other holder's length is unchanged");
    PROVE(keep->header.rc == 2,
          "and neither reference was consumed: the in-place write does not participate in "
          "refcounting at all");

    // ---- the second holder observes a write it never made ----
    //
    // This is the claim the model exists for. It is the precondition's consequence, and
    // asserting it is what makes a future weakening of either side of the contract fail
    // loudly instead of silently changing what shipped.
    elem* seen = (elem*)neon_list_at(keep, (int64_t)i);
    PROVE(seen->id == FRESH,
          "the second holder observes the write: this is the precondition's consequence, "
          "not desirable behaviour -- `ir::unique` must never emit this call for a list "
          "with another holder");
    PROVE(seen->tag == ELEM_TAG(FRESH),
          "and observes all sz bytes of it, so the corruption is a whole element rather "
          "than a torn one");

    // The slot that was not written is still intact for both holders -- the damage is
    // exactly one slot wide, which is what makes it so quiet in practice.
    for (unsigned k = 0; k < N; k++) { // constant bound, rule 3
        if (k == i) continue;
        elem* o = (elem*)neon_list_at(keep, (int64_t)k);
        PROVE(o->id == k && o->tag == ELEM_TAG(k),
              "every slot the write did not target is untouched for the second holder, so "
              "nothing but the written slot distinguishes a corrupted list from a sound one");
    }

    neon_release((neon_header*)l);
    PROVE(keep->header.rc == 1, "releasing one holder leaves the other alive");
    neon_release((neon_header*)keep);
    return 0;
}
