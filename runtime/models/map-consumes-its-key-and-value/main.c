// Model: keys and values are neither leaked nor released twice.
//
// THE INVARIANT: the ownership rules `neon/map.h` states actually hold --
//
//   * `set`, `remove` and `contains` CONSUME their key. The caller hands over one
//     reference and does not release it afterwards;
//   * `set` moves the key into the table, or -- when the table already holds that key --
//     drops the incoming one, and drops the value it overwrites;
//   * `remove` drops both the key it stored and its value;
//   * `find` and `at` BORROW. They are reached through `Op::Index`, whose operands the
//     refcount pass releases itself, so releasing here too would double-free.
//
// Rule 7 is the whole point of this model: the witness refcounts a box and has a real
// `release`. With a scalar key -- `release` NULL, which is what an `i64` map gets -- every
// one of these bugs is invisible, and that is exactly how "72 bytes leaked per lookup for
// a `List` key" shipped. `box_release` also asserts the count was not already zero, so an
// over-release fails at the call in map.c that made it.
//
// Verifies `src/map.c` compiled from source; see rule 1.
//
// VALIDATED BY MUTATION (rule 6). Deleting the `neon_map_release_key` call from
// `neon_map_set`'s found branch -- the leak that is invisible for an `i64` key and costs
// an allocation per call for a `str` -- was confirmed to fail this model on four separate
// claims, starting with "an overwrite drops the incoming key and the value it replaced,
// and nothing else", and then reverted.
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

int main(void) {
    hash_init();
    neon_map* m = &map_a;
    map_init(m, ctrl_a, keys_a, vals_a);

    // Insert: both boxes are moved into the table, neither is released.
    box* k = box_new(0);
    box* v = box_new(10);
    m = neon_map_set(m, &k, &v);
    pin_len(m, 1);
    PROVE(boxes_freed == 0, "an insertion releases nothing: both boxes moved into the map");

    // Overwrite: the table keeps the key it has, so the incoming key and the replaced
    // value are both the map's to drop -- and exactly those two.
    box* k2 = box_new(0);
    box* v2 = box_new(11);
    m = neon_map_set(m, &k2, &v2);
    pin_len(m, 1);
    PROVE(boxes_freed == 2,
          "an overwrite drops the incoming key and the value it replaced, and nothing else");

    // `contains` consumes its key and the map.
    box* k3 = box_new(0);
    neon_retain((neon_header*)m);
    PROVE(neon_map_contains(m, &k3), "the key is present");
    PROVE(boxes_freed == 3, "contains drops the key it was handed");

    // A miss must drop its key too -- the leak that is invisible for an i64 and costs a
    // whole allocation per call for a `str`.
    box* k4 = box_new(3);
    neon_retain((neon_header*)m);
    PROVE(!neon_map_contains(m, &k4), "the key is absent");
    PROVE(boxes_freed == 4, "contains drops its key on a miss as well as on a hit");

    // `find` borrows: the key must survive the call, and so must the stored value.
    box* k5 = box_new(0);
    void* slot = neon_map_find(m, &k5);
    PROVE(slot != NULL, "found");
    PROVE(k5->rc == 1, "find does not release the key it borrowed");
    PROVE((*(box**)slot)->rc == 1, "and does not touch the value it returns");
    box_release((void*)&k5); // the caller's to drop, since find borrowed it
    PROVE(boxes_freed == 5, "and the caller can drop it exactly once");

    // `remove` drops the stored key and the stored value, plus the key handed in.
    box* k6 = box_new(0);
    m = neon_map_remove(m, &k6);
    pin_len(m, 0);
    PROVE(boxes_freed == 8,
          "remove drops the stored key, the stored value, and the key it was handed");

    // Nothing is left in the table, so the books balance with no teardown at all.
    PROVE(boxes_made == boxes_freed,
          "every box made is released exactly once: nothing leaked, nothing double-released");
    return 0;
}
