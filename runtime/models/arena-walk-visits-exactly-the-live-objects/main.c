// Model: neon_arena_walk visits EXACTLY the live objects — every live one once, no freed
// slot, across both a chunk (small objects) and the big list.
//
// THE INVARIANT: after allocating small x, small y, and a big block, then freeing y, a walk
// visits x and big and not y, and visits nothing else. This is the property a dying fiber's
// teardown depends on: it must release each live object's outgoing references exactly once
// (miss one → a resource or shared value leaks) and must never touch a freed slot (touch one
// → use-after-free on memory already on the free list).
//
// Why the walk is walkable at all, and why CBMC can check it: a freed slot keeps its class in
// `flags` and only sets NEON_ALLOC_FREED, threading the free list through the `drop` field
// instead of clobbering the header — so the walk reads each slot's class to step past it and
// its FREED bit to skip it. The arena is bounded (grow is a single `if`; the walk's loops are
// over the handful of slots and big nodes this harness makes), so this runs the real walk.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Baseline: all properties green. Mutations, each reverted after:
//
// 1. neon_arena_free not setting NEON_ALLOC_FREED: the walk sees y as live — fails "y (freed)
//    is not visited" and "exactly two live objects are visited" (count 3).
// 2. neon_arena_walk's `!(flags & NEON_ALLOC_FREED)` guard removed (visit freed too): same
//    failures, from the walk side rather than the free side.
// 3. neon_arena_walk dropping the big-list loop: fails "the big block is visited" and the
//    count (2 → 1) — the arm the C tests reach under ASan, reached here directly.
//
// ---- SCOPE ----
//
// 1. THREE OBJECTS, ONE CHUNK. Enough to reach a live slot, a freed slot, and a big node;
//    multi-chunk stepping (the retired-chunk high-water) is the C test's job (arena_test.c,
//    concrete, ASan-backed). 2. The visitor only records identity; it does not itself free or
//    allocate, which is the walk's stated precondition. 3. SINGLE ARENA, SINGLE THREAD.

#include "../support/cbmc_support.h"
#include "libneon_rt.h"

#include <stdio.h>
#include <stdlib.h>

int fprintf(FILE* stream, const char* fmt, ...) { (void)stream; (void)fmt; return 0; }
int fflush(FILE* stream) { (void)stream; return 0; }

static void no_drop(void* p) { (void)p; }

static neon_header* g_x;
static neon_header* g_y;
static neon_header* g_big;
static int g_count;
static _Bool g_saw_x, g_saw_y, g_saw_big;

static void visit(neon_header* h, void* ctx) {
    (void)ctx;
    g_count++;
    if (h == g_x) g_saw_x = 1;
    if (h == g_y) g_saw_y = 1;
    if (h == g_big) g_saw_big = 1;
}

int main(void) {
    neon_arena* a = neon_arena_create();

    g_x = (neon_header*)neon_arena_alloc(a, 24, no_drop);    // small, class slot
    g_y = (neon_header*)neon_arena_alloc(a, 24, no_drop);    // small, class slot
    g_big = (neon_header*)neon_arena_alloc(a, 2000, no_drop); // > 512 → big list

    PROVE(g_x != g_y, "the two small objects are distinct slots");
    PROVE((g_big->flags & NEON_ALLOC_BIG) != 0, "the big block is on the big list");

    neon_arena_free(a, g_y); // y is now a freed slot the walk must skip
    PROVE((g_y->flags & NEON_ALLOC_FREED) != 0, "freeing marks the slot dead but keeps it");

    neon_arena_walk(a, visit, NULL);

    PROVE(g_saw_x, "x (live) is visited");
    PROVE(g_saw_big, "the big block (live) is visited");
    PROVE(!g_saw_y, "y (freed) is not visited");
    PROVE(g_count == 2, "exactly two live objects are visited, no more");

    neon_arena_drop(a);
    return 0;
}
