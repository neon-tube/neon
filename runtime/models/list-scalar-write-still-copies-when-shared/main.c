// Model: `neon_list_set_scalar` -- the copy-on-write cousin of the in-place write --
// against a list with a second holder.
//
// THE INVARIANT: with `rc > 1`, `neon_list_set_scalar` copies before it writes. It returns a
// list that is NOT the one it was given, and the other holder's buffer, length and element
// bytes are all exactly as they were.
//
// `neon_list_set_scalar` and `neon_list_set_scalar_inplace` are the same three lines apart
// from one call: the former routes through `neon_list_ensure_unique`, the latter does not.
// That single difference is the entire boundary between a write the compiler may perform
// blind and a write that must not be. It is easy to lose. The pair are adjacent in list.c,
// have near-identical names and identical signatures but for the return type, and
// `ir::unique`'s rewrite consists of swapping one for the other -- so a rewrite that fires
// one call too eagerly, or a hand edit that "simplifies" the duplication away, changes
// copy-on-write into shared mutation with no compile error and no output difference until
// somebody reads the other holder.
//
// This model is the positive half of that pair. Its negative half is
// `list-in-place-write-on-a-shared-list-corrupts-it`, which pins down what the in-place
// version does in exactly this state. Read together they say: the difference between the
// two functions is real, is observable, and is the difference that was intended.
//
// Verifies `src/list.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// `neon_list_set_scalar`'s `l = neon_list_ensure_unique(l)` call deleted, so the
// copy-on-write write mutates a shared list in place -- which is to say, the mutation turns
// the safe function into the unsafe one. This is the edit somebody makes after reading the
// `set_scalar_inplace` performance argument and generalising it one function too far.
//
// Confirmed to fail on six claims (6 of 900), starting with "writing to a shared list
// returns a different list: `neon_list_set_scalar` copies instead of mutating in place",
// and including "and the copy has its own buffer, so the two lists share no storage at
// all", "the copy is sole-owned, so the next write can be in place", "the copy released the
// reference it consumed exactly once, leaving the other holder sole owner", and both
// element claims on the other holder's buffer ("every element the other holder had is still
// the one it had" / "the write did not reach the other holder's buffer"). Reverted.
//
// The cost if shipped is silent and unbounded: every list value in the language stops being
// a value. `xs = ys` followed by a write to `xs` would change `ys`, with no crash, no
// diagnostic, and no output difference until `ys` is read -- and the refcount claim shows it
// would additionally corrupt the count, so the aliasing would eventually be joined by a
// double free.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. THE ELEMENT TYPE IS A SCALAR, AND THAT IS THE CORRECT DOMAIN -- a deliberate,
//    reasoned departure from rule 7, not an oversight, and it must not be "fixed".
//    `neon_list_set_scalar` carries the documented precondition (list.c:103-104) that the
//    element type is NOT refcounted: unlike `neon_list_set` it does not release the slot it
//    overwrites, so calling it for a counted element leaks the value being displaced. A
//    refcounted element is therefore outside this function's contract -- it is
//    `neon_list_set`'s domain, and the copy-on-write behaviour of *that* function with a
//    counted element is already covered by the older `runtime/models/list` harness. The
//    witness here has `retain` and `release` NULL, exactly as codegen emits for an `i64`.
//
//    Rule 7's actual demand -- exercise the case that makes the bug visible -- is met by
//    the *shape* of the element instead of by its ownership: 16 bytes, deliberately not a
//    machine word, with `tag` derived from `id`. The bugs reachable here are a copy of the
//    wrong width and a write at the wrong offset, and a 16-byte self-checking element makes
//    either visible as a torn or displaced payload where an `int64_t` would not.
//
//    Consequently NOT proved here: that the copy retains each shared element. With a scalar
//    witness `ensure_unique`'s retain loop is correctly skipped, so there is nothing to
//    prove; the counted case is the older list harness's `scenario_shared_cow`.
//
// 2. THE LENGTH IS CONCRETELY 2, and this is forced, not a convenience. This model reaches
//    `neon_list_ensure_unique`, which does `memcpy(c->data, l->data, l->len * sz)`. CBMC's
//    built-in `memcpy` is imprecise when the byte count is symbolic -- it leaves the copied
//    bytes unconstrained, and every property about the copy's contents then fails
//    spuriously (this is not Neon-specific; it reproduces in twenty lines of plain C, and
//    the old list harness records it at length as its "shape requirement 2"). So the
//    fixture is entered at a literal length. Two is the least that gives a written slot and
//    an unwritten neighbour to check the copy against. The *index* stays symbolic, because
//    it only ever appears as an offset, never as a count.
//
//    Not covered, therefore: lengths beyond two, by symbolic reasoning. Raising the bound
//    means adding a concrete arm, not relaxing a constraint.
//
// 3. `rc` IS EXACTLY 2. `neon_list_ensure_unique` branches on `rc == 1` and nothing finer,
//    so every count above one takes the identical path. The `rc == 1` arm -- where the same
//    call must return the *same* list and take no copy -- is the sole-owned case, covered
//    by `list-in-place-write-lands-in-the-right-slot` for the in-place function; the
//    generic `neon_list_set` version of it is in the older list harness.
//
// 4. Out-of-range indices trap before `ensure_unique` is reached; the trap is
//    `list-in-place-write-lands-in-the-right-slot`'s subject and this model enters in range.
//
// 5. Out-of-memory is not a recoverable path in this runtime -- every allocation failure
//    reaches `neon_trap`, which `_exit`s. CBMC does take those branches under
//    `--malloc-fail-null` and proves nothing is dereferenced before the trap, but a leak
//    check cannot fire past a trap, so "no leak on OOM" is vacuous by design rather than
//    proved.

#include "../support/cbmc_support.h"

#include <stdio.h>

#include "libneon_rt.h"

// Rule 4. `neon_trap` calls `fflush`/`fprintf`, and this model reaches a trap from every
// allocation check -- including the two inside the copy `ensure_unique` takes, both of which
// `--malloc-fail-null` opens. CBMC's models of stdio pull a `FILE` and its buffers into each
// of those sites. What a trap prints is not a property of the list, and the trap still
// terminates the path via `_exit`.
int fprintf(FILE* stream, const char* fmt, ...) { (void)stream; (void)fmt; return 0; }
int fflush(FILE* stream) { (void)stream; return 0; }

// ---- the element type and its witness ----
//
// 16 bytes with a self-checking tag, `retain`/`release` NULL. See SCOPE note 1: the scalar
// element is this function's documented domain, and the reasoning is on the page so that a
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

// Concrete; see SCOPE note 2.
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

    // A second holder. `neon_list_set_scalar` consumes one of the two references and must
    // now copy rather than write through.
    neon_retain((neon_header*)l);
    neon_list* keep = l;
    char* keep_data = l->data;
    size_t keep_len = l->len;
    PROVE(l->header.rc == 2, "the list is shared, so the write must copy");

    unsigned i = NONDET_UPTO(N - 1, "the slot written; both slots of the fixture are "
                                    "explored, so the copy is checked with the write at "
                                    "the front and at the back of the buffer");

    staging.id = FRESH;
    staging.tag = ELEM_TAG(FRESH);
    neon_list* mut = neon_list_set_scalar(l, (int64_t)i, &staging, ELEM_SZ);

    // ---- the write went somewhere else ----
    PROVE(mut != keep,
          "writing to a shared list returns a different list: `neon_list_set_scalar` "
          "copies instead of mutating in place");
    PROVE(mut->data != keep_data,
          "and the copy has its own buffer, so the two lists share no storage at all");
    PROVE(mut->len == keep_len, "the copy has the same length as the list it came from");
    PROVE(mut->len <= mut->cap, "the copy maintains len <= cap");
    PROVE(mut->w == &ELEM_W, "the copy carries the element witness of the original");
    PROVE(mut->header.rc == 1, "the copy is sole-owned, so the next write can be in place");

    // ---- the other holder is exactly as it was ----
    PROVE(keep->data == keep_data,
          "the other holder's buffer is neither moved nor freed by the write");
    PROVE(keep->len == keep_len, "the other holder's length is unchanged");
    PROVE(keep->header.rc == 1,
          "the copy released the reference it consumed exactly once, leaving the other "
          "holder sole owner");
    for (unsigned k = 0; k < N; k++) { // constant bound, rule 3
        elem* o = (elem*)neon_list_at(keep, (int64_t)k);
        PROVE(o->id == k, "every element the other holder had is still the one it had");
        PROVE(o->tag == ELEM_TAG(k),
              "and all of its bytes: the write did not reach the other holder's buffer");
    }

    // ---- the copy carries the write, and carries the rest verbatim ----
    elem* s = (elem*)neon_list_at(mut, (int64_t)i);
    PROVE(s == (elem*)(mut->data + (size_t)i * ELEM_SZ),
          "the write addresses data + i * sz in the copy, not in the original");
    PROVE(s->id == FRESH && s->tag == ELEM_TAG(FRESH),
          "the copy's slot i holds the whole new element");
    for (unsigned k = 0; k < N; k++) { // constant bound, rule 3
        if (k == i) continue;
        elem* o = (elem*)neon_list_at(mut, (int64_t)k);
        PROVE(o->id == k && o->tag == ELEM_TAG(k),
              "every slot the copy did not overwrite holds the original's bytes, so the "
              "copy is a faithful one and not merely a fresh buffer");
    }

    neon_release((neon_header*)mut);
    PROVE(keep->len == keep_len && keep->data == keep_data,
          "dropping the copy leaves the other holder's buffer alive: the copy owned its "
          "own storage");
    neon_release((neon_header*)keep);
    return 0;
}
