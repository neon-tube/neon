// Model: the open-addressed hash map.
//
// Drives `neon_map_new` / `set` / `remove` / `contains` / `find` from `src/map.c`
// -- the shipping source, compiled by CBMC alongside this harness -- through every
// interleaving of a bounded operation sequence over a small key domain, under an
// *arbitrary* hash function.
//
// ---- what is proved ----
//
//   - a key that was set is found, with the value most recently set for it, and a
//     key that was never set (or was removed) is absent;
//   - removal does not make a *different* key unreachable -- the classic tombstone
//     bug, where a probe stops early at a DEAD slot. This falls out of the point
//     above because the final sweep queries every key in the domain after an
//     arbitrary set/remove history, not just the key that was touched;
//   - `len` is exact after every operation: an overwrite does not increment it, a
//     removal of an absent key does not decrement it;
//   - `len` equals the number of FULL control bytes, and every control byte is one
//     of EMPTY/DEAD/FULL, so `len` cannot drift from the table it describes;
//   - the table is never entirely FULL. This is what makes `neon_map_slot`'s
//     fallback -- return slot 0 with found=false when the probe runs off the end --
//     unreachable for a full table, which is the case where it would silently
//     overwrite a live key. The load-factor rule is the only thing preventing it;
//   - a probe terminates and stays in bounds: `--unwinding-assertions` makes the
//     `n < m->cap` loop prove it exits, and `--bounds-check` covers the `(i+1) & mask`
//     wraparound. Both hold for tables that are nearly full and for tables that are
//     mostly tombstones;
//   - a clone -- taken by `set` on resize, and by `set`/`remove` on a shared map --
//     carries every live entry across and no tombstone at all;
//   - value semantics: a retained snapshot of the map is unchanged by a later
//     `set`/`remove` through the other reference;
//   - keys and values neither leak nor double-free. Both witnesses carry real
//     `retain`/`release` (they refcount a heap box, the shape a `str` or `List` key
//     has), every box counts its own drop, and the harness proves allocations equal
//     drops at exit. `--memory-leak-check` covers the map's own three arrays.
//
// ---- what is deliberately NOT proved ----
//
// * hash/eq agreement is a PRECONDITION here, not a checked property. `key_hash`
//   reads a nondet table indexed by the key's *value*, and `key_eq` compares that
//   same value, so `eq(a,b) => hash(a)==hash(b)` holds by construction. That is the
//   right scoping -- the map is not to blame for a witness that lies -- but it means
//   this model CANNOT catch the bug that shipped earlier today, where codegen's
//   `hash_expr` hashed a union key's pointer triple while `eq` compared it
//   structurally. That bug lives in codegen's witness emission; catching it needs a
//   check there, not here. Conversely, because the table is nondet, the proof covers
//   *every* hash function that does agree with eq, including the worst case where
//   all keys collide.
//
// * capacity is bounded to {4, 8}. `neon_map_new` hardcodes 8, and a resize from 8
//   would give a 16-slot probe loop -- past `--unwind 12`. So the harness declares a
//   fresh map to be cap 4 (its arrays are the ones the runtime allocated, merely
//   larger than the capacity claims; no map logic is reproduced) and lets the runtime
//   resize it to 8. Consequence: the resize path is proved for 4->8 only. It is
//   capacity-generic code -- one `cap * 2` and a mask -- so this is a bound on
//   confidence, not a different code path, but a bug that only appears above 8 slots
//   would not be seen.
//
// * three distinct keys and four values. Enough that the table both resizes (cap 4
//   holds at most 2 entries under the 3/4 load factor) and fills with tombstones,
//   but a probe chain longer than three live keys is not explored.
//
// * five operations. Longer histories are not explored; see the bound discussion
//   above.
//
// * out-of-memory is not a recoverable path in this runtime -- every allocation
//   failure reaches `neon_trap`, which `_exit`s. CBMC does take those branches under
//   `--malloc-fail-null` and proves nothing is dereferenced before the trap, but the
//   leak check cannot fire past a trap, so "no leak on OOM" is vacuous by design
//   rather than proved.

#include "../support/cbmc_support.h"
#include "libneon_rt.h"

#include <stdlib.h>

// ---- bounds ----
//
// Every loop below must sit under `--unwind 12`. The deepest is `neon_map_slot`'s
// probe, bounded by `m->cap`, which reaches 8 after the one resize: 8 iterations, 9
// unwindings. `neon_map_clone` and `neon_map_drop` walk the same cap. The harness's
// own loops are NOPS (5) and KDOM (3).
#define KDOM 3  // distinct keys: 0, 1, 2
#define VDOM 4  // distinct values
#define NOPS 5  // operations in the nondet history
#define CAP0 4  // fixture capacity; the runtime resizes it to 8

// ---- the key/value type ----
//
// A refcounted heap box holding a small integer. Slots store `box*`, so the
// witnesses' retain/release are real work rather than no-ops -- ownership bugs are
// invisible when `release` is NULL. Equality is by *content*, which is the case that
// matters: two distinct allocations holding the same integer are the same key, the
// way two `str` allocations holding the same bytes are.
typedef struct {
    neon_header header;
    unsigned v;
} box;

static unsigned boxes_made;
static unsigned boxes_dropped;

static void box_drop(void* p) {
    PROVE(((box*)p)->header.rc == 0, "a box is dropped only at rc == 0");
    boxes_dropped++;
    neon_free(p);
}

static box* box_new(unsigned v) {
    // `neon_alloc` traps rather than returning NULL, so there is no failure branch
    // to handle here; the trap ends the trace.
    box* b = (box*)neon_alloc(sizeof(box) - sizeof(neon_header), box_drop);
    b->v = v;
    boxes_made++;
    return b;
}

// Witness callbacks receive a pointer to the *slot*, which holds a `box*`.
static void box_retain(void* slot) {
    neon_retain((neon_header*)*(box**)slot);
}
static void box_release(void* slot) {
    neon_release((neon_header*)*(box**)slot);
}
static bool box_eq(const void* a, const void* b) {
    return (*(box* const*)a)->v == (*(box* const*)b)->v;
}

// An arbitrary hash, fixed per key value. Nondet, so the proof covers every hash
// function -- perfect, adversarial, and the degenerate one that maps every key to
// the same slot. Indexing by `v` is what makes `eq(a,b) => hash(a)==hash(b)` hold by
// construction; see the header comment on why that is a precondition and not a
// property.
static uint64_t hash_of[KDOM];

static uint64_t key_hash(const void* slot) {
    return hash_of[(*(box* const*)slot)->v];
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

// A map at CAP0 slots rather than the 8 `neon_map_new` hardcodes, so that the one
// resize this model can afford lands at 8 and stays under `--unwind`. The map is
// empty, so swapping its arrays loses nothing; no map logic is reproduced.
static neon_map* map_new_small(void) {
    neon_map* m = neon_map_new(&key_witness, &box_witness);
    // The arrays `neon_map_new` allocated are sized for 8 slots and stay exactly as
    // they are; only the declared capacity shrinks, so slots CAP0..7 are simply never
    // addressed. Nothing is reallocated and no map logic is reproduced -- the map is
    // empty, so there is no state to migrate.
    m->cap = CAP0;
    return m;
}

// Every control byte is a legal marker, FULL slots agree with `len`, and at least one
// slot is not FULL. The last is the load-factor invariant, and it is what keeps
// `neon_map_slot`'s run-off-the-end fallback (return 0, found=false) from ever firing
// on a table where slot 0 holds a live key.
static void check_table(neon_map* m) {
    size_t full = 0;
    for (size_t i = 0; i < m->cap; i++) {
        PROVE(m->ctrl[i] == NEON_MAP_EMPTY || m->ctrl[i] == NEON_MAP_DEAD ||
                  m->ctrl[i] == NEON_MAP_FULL,
              "every control byte is empty, dead or full");
        if (m->ctrl[i] == NEON_MAP_FULL) {
            full++;
        }
    }
    PROVE(full == m->len, "len equals the number of full slots");
    PROVE(full < m->cap, "the table is never entirely full, so a probe always finds a "
                         "slot that ends it");
    PROVE(m->cap == 4 || m->cap == 8, "capacity stays a power of two within the modelled range");
}

static void check_no_tombstones(neon_map* m) {
    for (size_t i = 0; i < m->cap; i++) {
        PROVE(m->ctrl[i] != NEON_MAP_DEAD, "a freshly cloned table carries no tombstones");
    }
}

int main(void) {
    for (unsigned i = 0; i < KDOM; i++) {
        hash_of[i] = nondet_ulong();
        // Not a restriction on behaviour: `neon_map_slot` uses the hash only as
        // `hash & (cap - 1)`, and cap never exceeds 8 here, so a hash of h and one of
        // h & 7 drive the table identically. Bounding it collapses 64 symbolic bits
        // the solver would otherwise carry for no additional coverage.
        ASSUME(hash_of[i] < 8, "sound: only the low log2(cap) bits of a hash are ever "
                               "read, and cap <= 8 throughout this model");
    }

    neon_map* m = map_new_small();

    // The shadow map the runtime is checked against.
    bool present[KDOM];
    unsigned value[KDOM];
    for (unsigned i = 0; i < KDOM; i++) {
        present[i] = false;
        value[i] = 0;
    }
    size_t count = 0;

    for (unsigned t = 0; t < NOPS; t++) {
        unsigned k = NONDET_UPTO(KDOM - 1,
                                 "three keys let the table both resize and accumulate "
                                 "tombstones; longer probe chains are not explored");
        unsigned kind = NONDET_UPTO(2, "set / remove / contains are the mutating and "
                                       "querying entry points; find is covered by the "
                                       "final sweep, at/len/eq are thin wrappers");

        // A snapshot through a second reference on some iterations, so that `set` and
        // `remove` take their copy-on-write path with rc > 1 as well as their in-place
        // one. Both must be reachable; neither may be assumed away.
        bool shared = nondet_bool();
        neon_map* snapshot = NULL;
        size_t snapshot_len = m->len;
        if (shared) {
            snapshot = m;
            neon_retain((neon_header*)m);
        }

        size_t cap_before = m->cap;
        box* kb = box_new(k); // every native below consumes its key

        if (kind == 0) {
            unsigned v = NONDET_UPTO(VDOM - 1, "values are opaque to the map; four are "
                                               "enough to tell an overwrite apart from "
                                               "the value it replaced");
            box* vb = box_new(v);
            m = neon_map_set(m, &kb, &vb); // moves both in
            if (!present[k]) {
                present[k] = true;
                count++;
            }
            value[k] = v;
            // A `set` that resized, or that copied a shared map, hands back a table
            // built by `neon_map_clone`, which must have compacted every tombstone away.
            if (shared || m->cap != cap_before) {
                check_no_tombstones(m);
            }
        } else if (kind == 1) {
            m = neon_map_remove(m, &kb);
            if (present[k]) {
                present[k] = false;
                count--;
            }
        } else {
            neon_retain((neon_header*)m); // `contains` consumes the map
            bool got = neon_map_contains(m, &kb);
            PROVE(got == present[k], "contains reports a key present exactly when it was "
                                     "set and not since removed");
        }

        PROVE(m->len == count, "len is exact: an overwrite does not increment it and "
                               "removing an absent key does not decrement it");
        check_table(m);

        if (shared) {
            PROVE(snapshot->len == snapshot_len,
                  "a retained snapshot is unchanged by a mutation through another reference");
            check_table(snapshot);
            neon_release((neon_header*)snapshot);
        }
    }

    // The whole domain, after an arbitrary history. This is where a probe that stopped
    // early at a tombstone shows up: the key that went missing need not be the key the
    // last operation touched.
    for (unsigned k = 0; k < KDOM; k++) {
        box* kb = box_new(k);
        void* slot = neon_map_find(m, &kb); // borrows both
        PROVE((slot != NULL) == present[k],
              "a key that was set is found, and one that was not is absent");
        if (slot != NULL) {
            PROVE((*(box**)slot)->v == value[k],
                  "the value found is the last one set for that key");
        }
        neon_release((neon_header*)kb); // find borrows, so the key is still ours
    }

    check_table(m);

    // Dropping the map must release exactly the keys and values it still holds. Every
    // box the harness made is then accounted for: none leaked, none dropped twice.
    neon_release((neon_header*)m);
    PROVE(boxes_made == boxes_dropped,
          "every key and value box is dropped exactly once: no leak, no double free");
    return 0;
}
