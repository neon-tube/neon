// Model: two maps holding the same entries are equal, whatever order they were built in.
//
// THE INVARIANT: `neon_map_eq` compares *contents*, not slot arrangement.
//
// An open-addressed table has no canonical order: the same two entries sit in different
// slots depending on which was inserted first, because the second one probes past the
// first when they collide. So `neon_map_eq` cannot walk the two slot arrays in step, and
// it does not -- it walks `a` and looks each key up in `b`, comparing lengths first so
// that "every key of a is in b" is enough. This model builds the same pair of entries in
// the two possible orders and requires the answer not to notice, and then perturbs one
// value and requires it to notice that.
//
// The hash is unconstrained, so the case where the two keys collide -- the only case where
// the two tables actually differ -- is covered rather than hoped for.
//
// Verifies `src/map.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// `neon_map_eq`'s body rewritten to walk the two slot arrays in step -- comparing
// `a->ctrl[i]` against `b->ctrl[i]` and `a->keys + i * ksz` against `b->keys + i * ksz`,
// instead of looking each of `a`'s keys up in `b` with `neon_map_find`. Confirmed to fail
// (3 of 2244 baseline properties) and reverted.
//
// It failed on exactly the claims the model is named for: "two maps holding the same
// entries are equal however they were arranged", "and equality does not depend on which
// side is walked", and "restoring the value restores equality". The two equal-but-differing
// maps in the fixture were built by different insertion histories on purpose, and that is
// the whole reason this mutation is visible here.
//
// This is a mutation worth taking seriously rather than a strawman: the parallel-walk
// version is faster, shorter, and obviously correct for any table with a canonical layout.
// Open addressing has none -- the same set of entries lands at different slots depending on
// insertion and removal order, so the rewrite makes `==` on maps depend on history that the
// language does not expose. Shipped, it would report two maps unequal that print
// identically, non-deterministically with respect to how each was built.
//
// Cost note for whoever validates this next: at ~13s solve time this is the slowest model
// in the suite, and the mutant run is comparable.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. HASH/EQ AGREEMENT IS A PRECONDITION, NOT A CHECKED PROPERTY. `key_hash` reads a
//    nondet table indexed by the key's *value* and the witness `eq` compares that same
//    value, so `eq(a, b) => hash(a) == hash(b)` holds by construction. That is correct
//    scoping -- a map cannot be blamed for a witness that lies about its own type --
//    but it means no model here can catch the bug that shipped on 2026-07-19, where
//    codegen's `hash_expr` hashed a union key's pointer triple while `eq` compared it
//    structurally, so `contains` returned false for a key that was present, with no
//    crash and no error. That bug is in codegen's witness emission and needs a check
//    there. In exchange, every hash function that *does* agree with eq is covered.
//
// 2. RESIZE, CLONE AND DROP ARE UNREACHABLE FOR ANY OF THESE MODELS, and this is a
//    limitation of the tool rather than a choice:
//
//      CBMC models a heap allocation as an untyped byte array, so EVERY field read back
//      out of a heap object is symbolic. That bites in two independent places, and the
//      second one is the one that actually decides the matter:
//
//        a) Function pointers. Every witness call in map.c goes through `m` --
//           `m->vw->release(...)`. CBMC resolves an indirect call by branching over every
//           address-taken function of matching type, and `neon_map_drop` is
//           `void (*)(void*)` exactly like a witness `release`, so CBMC believes
//           `m->vw->release` may be `neon_map_drop` and recurses into it ~12 deep.
//
//        b) Sizes and capacities. `neon_map_drop` (map.c:14) and `neon_map_slot`
//           (map.c:48) both loop on `n < m->cap`, and the body indexes with
//           `m->kw->value->size` / `m->vw->size` -- so the loop bounds are symbolic and
//           each iteration does symbolic-stride pointer arithmetic into a malloc'd byte
//           array under `--pointer-check`.
//
//      Measured: three `set`s triggering one resize did not finish in 400s; the same
//      harness on a *statically* allocated map finished in 0.25s, because a static object
//      has typed fields and both facets resolve.
//
//    Hence a static fixture with `rc` 1, kept under the load factor so no clone is taken.
//    Not verified anywhere in this set, therefore: "a resize preserves every live entry
//    and drops every tombstone", and "set/remove copy before mutating when rc > 1".
//
//    DO NOT reach for `goto-instrument --restrict-function-pointer`. It was tried on CBMC
//    6.10.0 and the result was negative, which is recorded here so it is not rediscovered:
//    the restriction applies cleanly and soundly (a wrong target becomes an `ASSERT false`
//    rather than a silent narrowing) and it does collapse facet (a) -- nested
//    `neon_map_drop` loop entries fell from 9 to 2 over an identical symex window. Heap
//    maps remained intractable regardless, because facet (b) is untouched and is on its
//    own sufficient: a harness doing ONE `set` and one `release` on a heap map, with the
//    restriction fully applied and OOM checks off, still timed out at 100s. The
//    "distinguishable types" escape hatch an earlier version of this note suggested would
//    not have worked either, for the same reason.
//
//    The one avenue not tried: harness-side assert-then-pin of `m->cap` and the witness
//    sizes, the trick `pin_len` already uses for `len`. Plausible -- but it pins the very
//    field the resize property is about (`cap` 8 -> 16), so it would need care not to
//    prove the property vacuously.
//
// 3. Capacity is 8 and cannot go lower: `neon_map_set` clones once
//    `(len + 1) * 4 >= cap * 3`, which at capacity 4 fires on the second entry, so a
//    smaller table would hold one live key and prove nothing about probing. The same
//    rule caps every model here at four live entries.
//
// 4. Out-of-memory is not a recoverable path in this runtime -- every allocation
//    failure reaches `neon_trap`, which `_exit`s. CBMC does take those branches under
//    `--malloc-fail-null` and proves nothing is dereferenced before the trap, but a
//    leak check cannot fire past a trap, so "no leak on OOM" is vacuous by design
//    rather than proved.

#include "../support/cbmc_support.h"
#include "libneon_rt.h"

#include <stdio.h>
#include <stdlib.h>

// Rule 4. `neon_trap` calls `fflush`/`fprintf`, every allocation check in map.c can
// reach a trap, and CBMC's models of those pull a `FILE` into each of those sites. The
// model has nothing to say about stdio.
int fprintf(FILE* stream, const char* fmt, ...) { (void)stream; (void)fmt; return 0; }
int fflush(FILE* stream) { (void)stream; return 0; }

// ---- the fixture ----
//
// Capacity 8, statically allocated, `rc` 1. All three choices are forced; see the
// SCOPE note in each model's header comment.
#define CAP 8
#define POOL 20

// A key/value is a refcounted box, not a scalar: rule 7. With a scalar the witness has
// no `release`, and every ownership bug in map.c becomes invisible. Equality is by
// *content*, so two distinct boxes holding the same integer are the same key -- the way
// two `str` allocations holding the same bytes are.
//
// The box is a bare count rather than a `neon_header`, and the pool is small, because a
// slot holds a `box*` that every `eq`/`hash`/`release` reads at a *symbolic* slot index:
// CBMC resolves each of those against the whole pool, so the pool's size in bytes lands
// straight in the formula. Going from 48 24-byte boxes to 20 8-byte ones took the
// original combined harness from "does not finish in 900s" to 8s.
typedef struct {
    unsigned rc;
    unsigned v;
} box;

static unsigned boxes_made;
static unsigned boxes_freed;
static box pool[POOL];
static unsigned pool_next;

static box* box_new(unsigned v) {
    PROVE(pool_next < POOL, "the box pool is large enough for the script");
    box* b = &pool[pool_next++];
    b->rc = 1;
    b->v = v;
    boxes_made++;
    return b;
}

// Witness callbacks receive a pointer to the *slot*, which holds a `box*`.
static void box_retain(void* slot) {
    (*(box**)slot)->rc++;
}

static void box_release(void* slot) {
    box* b = *(box**)slot;
    // The over-release oracle, sharper than CBMC's own double-free check: it fails at
    // the call in map.c that released once too often, not at some later use.
    PROVE(b->rc > 0, "no key or value is released after its count reached zero");
    if (--b->rc == 0) {
        boxes_freed++;
    }
}

static bool box_eq(const void* a, const void* b) {
    return (*(box* const*)a)->v == (*(box* const*)b)->v;
}

// An arbitrary hash, fixed per key value. Unconstrained, so every property below is
// checked for *every* hash function at once -- a perfect one, one that sends both keys
// to the same bucket, and one that sends them to the last slot so every probe wraps.
// Indexing by the key's value is what makes `eq(a,b) => hash(a)==hash(b)` hold by
// construction; see the SCOPE note.
static uint64_t hash_of[4];

static uint64_t key_hash(const void* slot) {
    return hash_of[(*(box* const*)slot)->v];
}

static void hash_init(void) {
    for (unsigned i = 0; i < 4; i++) {
        hash_of[i] = nondet_ulong();
        ASSUME(hash_of[i] < CAP,
               "sound, not a scoping choice: `neon_map_slot` reads a hash only as "
               "`hash & (cap - 1)` and cap is 8 here, so h and h & 7 drive the table "
               "identically. It drops 61 symbolic bits the solver would carry for free");
    }
}

static const neon_witness box_witness = {
    .size = sizeof(box*),
    .retain = box_retain,
    .release = box_release,
    .eq = box_eq,
    .cmp = NULL,
};

static const neon_key_witness key_witness = {
    .value = &box_witness,
    .hash = key_hash,
    .eq = box_eq,
};

static void map_never_dropped(void* p) {
    (void)p;
    PROVE(false, "the fixture map is never dropped: its count never reaches zero");
}

static neon_map map_a;
static unsigned char ctrl_a[CAP];
static box* keys_a[CAP];
static box* vals_a[CAP];

static void map_init(neon_map* m, unsigned char* ctrl, box** keys, box** vals) {
    m->header.rc = 1;
    m->header.flags = 0;
    m->header.drop = map_never_dropped;
    m->kw = &key_witness;
    m->vw = &box_witness;
    m->len = 0;
    m->cap = CAP;
    m->ctrl = ctrl;
    m->keys = (char*)keys;
    m->vals = (char*)vals;
    for (size_t i = 0; i < CAP; i++) { // constant bound, rule 3
        ctrl[i] = NEON_MAP_EMPTY;
    }
}

// Assert-then-pin. CBMC computes `len` as `found ? len : len + 1` and keeps it
// symbolic, which leaves map.c's load-factor test `(len + 1) * 4 >= cap * 3`
// symbolically reachable even though it is false for a table this small -- and that
// drags in the clone path the SCOPE note explains cannot be symexed. Writing back a
// value that has just been *proved* equal hides nothing: a `len` off by one fails the
// PROVE before the assignment runs.
static void pin_len(neon_map* m, size_t expect) {
    PROVE(m->len == expect, "len tracks the entries the map holds");
    m->len = expect;
}

static neon_map map_b;
static unsigned char ctrl_b[CAP];
static box* keys_b[CAP];
static box* vals_b[CAP];

static void put(neon_map* m, unsigned k, unsigned v, size_t len_after) {
    box* kb = box_new(k);
    box* vb = box_new(v);
    neon_map* r = neon_map_set(m, &kb, &vb);
    PROVE(r == m, "the fixture is updated in place");
    pin_len(m, len_after);
}

int main(void) {
    hash_init();
    neon_map* a = &map_a;
    neon_map* b = &map_b;
    map_init(a, ctrl_a, keys_a, vals_a);
    map_init(b, ctrl_b, keys_b, vals_b);

    // The same two entries, inserted in opposite orders. When the hashes collide these
    // two tables put the same pair of keys in swapped slots.
    put(a, 0, 10, 1);
    put(a, 1, 11, 2);
    put(b, 1, 11, 1);
    put(b, 0, 10, 2);

    PROVE(neon_map_eq(a, a), "a map equals itself");
    PROVE(neon_map_eq(a, b),
          "two maps holding the same entries are equal however they were arranged");
    PROVE(neon_map_eq(b, a), "and equality does not depend on which side is walked");

    // Change one value. Same keys, same length, different contents.
    put(b, 0, 12, 2);
    PROVE(!neon_map_eq(a, b), "maps differing in one value are not equal");
    PROVE(!neon_map_eq(b, a), "in either direction");

    // Same keys again, and equal once more -- so the disagreement above was the value
    // and not something the extra `set` did to the table.
    put(b, 0, 10, 2);
    PROVE(neon_map_eq(a, b), "restoring the value restores equality");

    // Differing lengths are the cheap case `neon_map_eq` checks first.
    box* r = box_new(1);
    b = neon_map_remove(b, &r);
    pin_len(b, 1);
    PROVE(!neon_map_eq(a, b), "a map with fewer entries is not equal to one with more");
    PROVE(!neon_map_eq(b, a), "in either direction");
    return 0;
}
