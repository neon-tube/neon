// Model: a fresh map is empty and every slot is EMPTY.
//
// THE INVARIANT: `neon_map_new` hands back a map with no entries, a power-of-two
// capacity, the witnesses it was given, a count of one, and -- the part worth a machine
// check -- a control array that is entirely EMPTY.
//
// That last one is not obvious from the source: the control bytes come from `calloc`, and
// `NEON_MAP_EMPTY` being 0 is what makes zeroed memory mean "empty". If either the marker
// or the allocator changed, every slot in a new map would read as a live entry and the
// first lookup would compare against uninitialised keys. Nothing else in map.c rechecks it.
//
// This is the only model in the set that touches a heap-allocated map, and it is why it
// does nothing else with it: one witness call on a heap map is what makes CBMC diverge
// (SCOPE note 2). It is torn down by hand rather than released, because releasing it would
// call exactly that drop.
//
// Verifies `src/map.c` compiled from source; see rule 1.
//
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
//      CBMC models a heap allocation as an untyped byte array, so a function pointer
//      read back out of a heap object is symbolic. Every witness call in map.c is one
//      -- `m->vw->release(...)`, reached through `m`. CBMC resolves an indirect call by
//      branching over every address-taken function of matching type, and `neon_map_drop`
//      is `void (*)(void*)`, exactly like a witness `release`. So on a heap-allocated
//      map CBMC believes `m->vw->release` may be `neon_map_drop`, recurses into it
//      twelve deep, and unwinds a loop bounded by a symbolic `m->cap` at every level.
//      Measured: three `set`s triggering one resize did not finish in 400s; the same
//      harness on a *statically* allocated map finished in 0.25s, because a static
//      object has typed fields and the call resolves.
//
//    Hence a static fixture with `rc` 1, kept under the load factor so no clone is
//    taken. Not verified anywhere in this set, therefore: "a resize preserves every
//    live entry and drops every tombstone", and "set/remove copy before mutating when
//    rc > 1". Reaching them needs `goto-instrument --restrict-function-pointer` in the
//    pipeline, or a runtime where a drop and a witness release have distinguishable
//    types.
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

    neon_map* m = neon_map_new(&key_witness, &box_witness);

    PROVE(m->len == 0, "a fresh map holds no entries");
    PROVE(m->cap == 8, "a fresh map has eight slots");
    PROVE((m->cap & (m->cap - 1)) == 0,
          "the capacity is a power of two, which `neon_map_slot`'s mask requires");
    PROVE(m->kw == &key_witness && m->vw == &box_witness,
          "a fresh map keeps the witnesses it was given");
    PROVE(m->header.rc == 1, "a fresh map is uniquely owned, so the first set is in place");
    PROVE(m->header.drop != NULL, "and carries a drop function");

    for (size_t i = 0; i < 8; i++) { // constant bound, rule 3
        PROVE(m->ctrl[i] == NEON_MAP_EMPTY,
              "every slot of a fresh map reads as empty, which relies on calloc's zeroes "
              "meaning NEON_MAP_EMPTY");
    }

    // By hand, not `neon_release`: see the header comment.
    free(m->ctrl);
    free(m->keys);
    free(m->vals);
    neon_free(m);
    return 0;
}
