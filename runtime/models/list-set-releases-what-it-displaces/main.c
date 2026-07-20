// Model: writes a fresh identity into every in-range slot of a sole-owned list and
// audits the refcount of every element in it.
//
// THE INVARIANT: `neon_list_set` leaves `len` unchanged, releases the element it
// displaces exactly once, installs the new one whole at `data + i * w->size`, and
// touches no other slot -- neither its bytes nor its refcount.
//
// "Exactly once" is the whole claim, and both directions of it have shipped as bugs in
// this shape of code. Release the displaced element zero times and every overwrite leaks
// it -- invisible for an `i64`, an allocation per call for a `str`. Release it twice, or
// release the slot *after* the `memcpy` rather than before, and the program frees an
// object it no longer owns; that is a use-after-free with no crash at the site that
// caused it. `list.c` gets this right by ordering `w->release(slot)` ahead of
// `memcpy(slot, elem, sz)`, and nothing else in the file rechecks that order.
//
// Neither direction is observable with a scalar element, whose witness has a NULL
// `release` so the whole branch is dead (rule 7). The element here is a 16-byte struct
// with a real retain/release keeping a per-identity count, and `elem_release` asserts
// the count is positive *before* decrementing -- so an over-release fails at the call in
// `list.c` that made it rather than at some later use. The 16-byte width, with a `tag`
// derived from the `id`, is what turns a slot-width bug into a payload at the wrong
// offset instead of a coincidentally-correct read.
//
// The "no other slot" half needs the audit over `k != i` below: a `set` that wrote at
// `data + i` with the wrong stride would still put the new element somewhere, and only
// reading the neighbours back shows it landed on one of them.
//
// Verifies `src/list.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Baseline: 908 properties, VERIFICATION SUCCESSFUL. Three mutations, each reverted.
//
// 1. The defect this model is named for: `if (l->w->release) l->w->release(slot)` deleted
//    from `neon_list_set`, so the overwritten element is dropped on the floor still
//    holding its reference. Failed 2 of 889, on "set releases the displaced element
//    exactly once" and "dropping the list after a set releases every element it holds
//    exactly once". A leak, not a crash -- which is exactly why it needs a model: no test
//    that only reads the list back would ever notice.
//
// 2. `char* slot = l->data + i * sz` -> `... + i * 8`, the literal-width bug in the write
//    path. Failed 10 of 908 -- on the displaced-release claim, "the new element's bytes
//    land whole in slot i", "set leaves every other slot's bytes alone", "set does not
//    touch any other element's refcount", and inside the witness on "release is handed a
//    well-formed element". The last is the useful one: the wrong slot is handed to
//    `release`, so the bug is a double-free of one element and a leak of another, not
//    merely a misplaced write.
//
// 3. The release aimed at the wrong operand: `l->w->release(slot)` -> `release(elem)`,
//    releasing the incoming element instead of the one it displaces. A plausible slip,
//    and both halves of the ownership transfer are wrong at once. Failed 4 of 908 on
//    "set releases the displaced element exactly once" AND "set takes ownership of the
//    new element exactly once" -- the model pins both directions, so it separates this
//    from mutation 1 rather than reporting the same failure twice.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. The list here is sole-owned, so `neon_list_ensure_unique` returns immediately.
//    NOT proved here: that `set` on a shared list copies first, and that the displaced
//    element then belongs to the *original* and must not be released -- that is
//    `list-mutating-a-shared-list-copies-it`'s claim, and it is a different property.
//
// 2. `set`'s out-of-range branch is the same guard `at` has, and is covered by
//    `list-at-traps-outside-the-list`. NOT proved here: that `set` traps outside the
//    list. The index is assumed in range below.
//
// 3. `neon_list_set_scalar` and `neon_list_set_scalar_inplace` are the specialised
//    writes codegen emits for a non-refcounted element. They are not driven here at all;
//    their preconditions (element not refcounted, list already sole-owned) are codegen's
//    and the optimiser's to keep, and they have their own models.
//
// 4. Lengths reach 3 and growth never runs, so NOT proved: that `set` addresses
//    correctly into a buffer that has been `realloc`ed.
//
// ---- Assumptions ----
//
// Two, both bound-encoding:
//
//   * `n >= 1` -- `set` needs an in-range index to exist.
//   * `i < n` -- the in-range half. The out-of-range branch traps and is SCOPE note 2.

#include "../support/cbmc_support.h"

#include <stdio.h>

#include "libneon_rt.h"

// Rule 4. `neon_trap` calls `fflush`/`fprintf`, and CBMC's models of those pull a `FILE`
// and its buffers in at *every* trap site -- reachable here from `set`'s bounds check and
// from every allocation check `--malloc-fail-null` opens. Left alone they account for
// most of the program's addressed objects and put it over CBMC's default
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
#define SMALLN 3
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
// push/set site is enough on its own to exceed the default `--object-bits`.
static elem staging;

static neon_list* push_owned(neon_list* l, uint64_t id) {
    staging.id = id;
    staging.tag = ELEM_TAG(id);
    live[id]++;
    return neon_list_push(l, &staging);
}

int main(void) {
    unsigned n = NONDET_UPTO(SMALLN,
        "list length; set writes one slot of an already-built list and never grows, so a "
        "larger bound buys nothing here");
    ASSUME(n >= 1, "set needs an in-range index to exist; the out-of-range branch is the "
                   "same trap list-at-traps-outside-the-list covers");
    unsigned i = NONDET_UPTO(SMALLN,
        "the index written; every in-range slot of every reachable length is explored");
    ASSUME(i < n, "the in-range half -- the out-of-range branch traps, see SCOPE note 2");

    neon_list* l = neon_list_new(&ELEM_W);
    for (unsigned k = 0; k < SMALLN; k++) { // constant bound, rule 3
        if (k >= n) break;
        l = push_owned(l, k);
    }

    // A fresh identity, distinct from everything already in the list, so displacing
    // element i is distinguishable from overwriting it with itself.
    const uint64_t fresh = NIDS - 1;
    staging.id = fresh;
    staging.tag = ELEM_TAG(fresh);
    live[fresh]++;
    l = neon_list_set(l, (int64_t)i, &staging);

    PROVE(l->len == n, "set leaves len unchanged");
    PROVE(l->len <= l->cap, "set maintains len <= cap");
    PROVE(live[i] == 0, "set releases the displaced element exactly once");
    PROVE(live[fresh] == 1, "set takes ownership of the new element exactly once");

    elem* s = (elem*)neon_list_at(l, (int64_t)i);
    PROVE(s == (elem*)(l->data + (size_t)i * ELEM_W.size),
          "the written slot is data + i * w->size");
    PROVE(s->id == fresh && s->tag == ELEM_TAG(fresh),
          "the new element's bytes land whole in slot i");

    for (unsigned k = 0; k < SMALLN; k++) { // constant bound, rule 3
        if (k >= n) break;
        if (k == i) continue;
        elem* o = (elem*)neon_list_at(l, (int64_t)k);
        PROVE(o->id == k && o->tag == ELEM_TAG(k),
              "set leaves every other slot's bytes alone");
        PROVE(live[k] == 1, "set does not touch any other element's refcount");
    }

    neon_release((neon_header*)l);
    for (unsigned k = 0; k < NIDS; k++) { // constant bound, rule 3
        PROVE(live[k] == 0,
              "dropping the list after a set releases every element it holds exactly once");
    }
    return 0;
}
