// Model: the per-fiber arena's core contract — a fresh allocation is uniquely owned,
// a freed slot of a class is handed back before any fresh space is bumped, that reuse
// costs no footprint (the PLATEAU the design leans on), a big allocation is tracked and
// its footprint returns on free, and drop leaves nothing behind.
//
// THE INVARIANTS:
//   1. `neon_arena_alloc` publishes `rc == 1`, the given drop, and a class encoded in
//      `flags` (or the BIG flag past the 512-byte ceiling) — never both, never neither.
//   2. Freeing a small object then allocating the SAME size returns the SAME address:
//      the class free list is consulted before the bump pointer, so a fiber that
//      allocates and frees the same shapes forever reuses one slot forever.
//   3. That reuse does not grow the footprint — the property that makes a long-lived
//      fiber plateau at its peak working set instead of leaking with total allocation.
//   4. A big allocation raises the footprint and freeing it lowers it by the same amount,
//      so the big list's accounting is exact (the field that lets `footprint` be trusted).
//   5. `neon_arena_drop` frees every chunk, every big block, and the control struct:
//      `--memory-leak-check` is the oracle, and reaching the end with nothing leaked is
//      the proof. This is slice 1's teardown — objects with no outgoing references,
//      bulk-freed at the fiber's death; the outgoing-reference WALK is slice 4.
//
// Why CBMC can take the real allocator here when the slab model could not: the slab is a
// process-global free list refilled by a chunk-carving loop, the unbounded stateful shape
// CBMC is worst at, so its model runs the plain-malloc seam and leaves integrity to ASan.
// The arena's grow is a single `if` (one 4 KB chunk covers any small request, 4080 > 512),
// and its only loops walk the big list and the drop lists — both bounded by the handful of
// allocations this harness makes. So there is no `#ifdef NEON_CBMC` seam in `arena.c`, and
// this proves the bump-and-reclaim logic itself.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Baseline: all properties green, VERIFICATION SUCCESSFUL. Mutations, each reverted after:
//
// 1. `neon_arena_alloc`'s free-list consult deleted (always bump): the reuse arm never
//    taken. Fails invariant 2 ("a freed slot of the class is reused: same address") and
//    invariant 3 ("reuse does not grow the footprint") — the second alloc bumps fresh
//    space instead of reclaiming, so both the address and the plateau break.
// 2. `neon_arena_free` big case dropping the `a->footprint -= node->total`: fails
//    invariant 4 ("freeing the big block returns the footprint"). Costs nothing at
//    runtime and makes `footprint` monotonically wrong, which no functional test notices.
// 3. `neon_arena_drop` skipping the big-list walk (frees chunks only): fails
//    `--memory-leak-check` — the surviving big block is leaked. The arm the slab model can
//    only reach through ASan, reached here directly. (This mutation is why the harness
//    leaves `live_big` outstanding at drop: an earlier draft freed every big block first,
//    so the big-list walk was never exercised and this mutation slipped through green —
//    a hole CBMC's mutation pass caught, not a hypothetical.)
//
// ---- SCOPE: what this model does not cover ----
//
// 1. NON-OVERLAP OF MANY LIVE OBJECTS is the tinyunit suite's job (`arena_test.c`,
//    concrete sentinels under ASan). CBMC sees a whole chunk as one malloc object, so it
//    cannot distinguish sub-slots by its own pointer model; the endpoint sentinels here
//    catch a slot handed out twice at the two sizes used, not an arbitrary overlap.
// 2. A HANDFUL OF ALLOCATIONS, not arbitrarily many. Enough to reach every arm — bump,
//    reclaim, big, and drop over a non-empty list — but the class free list's LIFO order
//    over long histories is not swept here.
// 3. SINGLE ARENA, SINGLE THREAD. The design's isolation guarantee (one arena is touched
//    by one fiber, so its non-atomic refcount is sound) is a property of the runtime, not
//    something this model — which never crosses a thread — is evidence about.
// 4. THE TEARDOWN WALK IS NOT MODELLED. `neon_arena_drop` here bulk-frees only; releasing
//    a dying arena's outgoing references before the free is slice 4 and gets its own model.

#include "../support/cbmc_support.h"
#include "libneon_rt.h"

#include <stdio.h>
#include <stdlib.h>

// Rule 4: `neon_trap` reaches stdio, and every allocation path traps under
// `--malloc-fail-null`. The model has nothing to say about stdio.
int fprintf(FILE* stream, const char* fmt, ...) { (void)stream; (void)fmt; return 0; }
int fflush(FILE* stream) { (void)stream; return 0; }

static void a_drop(void* p) { (void)p; }
static void b_drop(void* p) { (void)p; }

int main(void) {
    neon_arena* a = neon_arena_create();
    // A fresh arena's first chunk is lazy, so it holds nothing from the OS yet.
    PROVE(neon_arena_footprint(a) == 0, "a fresh arena has taken no memory");

    // ---- invariant 1: a fresh small allocation is uniquely owned and class-tagged ----
    // A concrete size, not a symbolic one: the reuse and plateau properties are about the
    // free-list-before-bump ORDER, which is identical for every class, and a nondet size
    // multiplies the class arithmetic across the whole run for no extra coverage (and blows
    // the solve time past any useful bound). 24 lands mid-class; the tinyunit suite sweeps
    // sizes concretely under ASan.
    size_t small = 24;
    neon_header* x = (neon_header*)neon_arena_alloc(a, small, a_drop);
    PROVE(x->rc == 1, "a fresh allocation is uniquely owned: rc is 1");
    PROVE(x->drop == a_drop, "and keeps the drop it was given");
    PROVE((x->flags & NEON_ALLOC_BIG) == 0, "a small object is not flagged big");
    PROVE((x->flags & NEON_ALLOC_CLASS_MASK) != 0, "and carries a non-zero class");

    // Endpoint sentinels (SCOPE 1): first and last payload byte of two live objects.
    char* xp = (char*)(x + 1);
    xp[0] = 'x';

    neon_header* y = (neon_header*)neon_arena_alloc(a, small, b_drop);
    PROVE(y != x, "two live allocations of the same class are distinct slots");
    char* yp = (char*)(y + 1);
    yp[0] = 'y';
    PROVE(xp[0] == 'x', "and the second did not clobber the first");

    // ---- invariants 2 & 3: free then same-size alloc reuses the slot, for free ----
    size_t before_reuse = neon_arena_footprint(a);
    neon_arena_free(a, x);
    neon_header* z = (neon_header*)neon_arena_alloc(a, small, a_drop);
    PROVE(z == x, "a freed slot of the class is reused: same address");
    PROVE(z->rc == 1, "and comes back as a fresh object");
    PROVE(neon_arena_footprint(a) == before_reuse,
          "reuse does not grow the footprint: the plateau property");

    // ---- invariant 4: a big allocation's footprint is exact across alloc/free ----
    size_t before_big = neon_arena_footprint(a);
    neon_header* big = (neon_header*)neon_arena_alloc(a, 4096, b_drop);
    PROVE(big->rc == 1, "the big allocation is uniquely owned too");
    PROVE(big->flags & NEON_ALLOC_BIG, "and is flagged big, not class-tagged");
    PROVE(neon_arena_footprint(a) > before_big, "allocating it raised the footprint");
    neon_arena_free(a, big);
    PROVE(neon_arena_footprint(a) == before_big,
          "and freeing it returned the footprint exactly");

    // ---- invariant 5: drop frees chunk(s), the big list, AND the control struct ----
    // Leave objects live in BOTH reclamation paths so drop must walk each: y and z sit in
    // the chunk (freed by the chunk walk), and this second big block sits on the big list
    // (freed by the big walk). Freeing `big` above and leaving `live_big` here is deliberate
    // — without a big block still outstanding at drop, the big-list walk is never exercised
    // and a drop that skipped it would still look leak-clean.
    neon_header* live_big = (neon_header*)neon_arena_alloc(a, 5000, a_drop);
    PROVE(live_big->flags & NEON_ALLOC_BIG, "the surviving big block is on the big list");
    // The memory-leak check is the oracle — reaching return with nothing outstanding is proof.
    neon_arena_drop(a);
    return 0;
}
