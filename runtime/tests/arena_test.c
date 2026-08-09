// `runtime/src/arena.c`: the per-fiber heap. Slice 1 of the fiber work (see
// `docs/design/fibers.md`) — a standalone, explicitly-passed arena. These pin the four
// properties the design leans on: distinct live objects never overlap, a freed slot comes
// back for reuse, the footprint PLATEAUS under alloc/free churn (it does not grow with total
// allocation), and drop frees everything (LeakSanitizer is the oracle for that last one).

#include "tinyunit.h"

#include "support.h"

#include "neon/arena.h"

TEST_SUITE("arena");

// A drop that only frees is wrong for an arena object — the arena owns the memory and frees
// it in bulk — so arena tests never install a real drop. NULL is fine; nothing here releases
// through the refcount path.
static void no_drop(void* p) {
    (void)p;
}

TEST(alloc_starts_at_one_with_the_given_drop) {
    neon_arena* a = neon_arena_create();
    neon_header* h = (neon_header*)neon_arena_alloc(a, 16, no_drop);
    EXPECT_EQ(h->rc, 1u);
    EXPECT_EQ(h->drop, no_drop);
    EXPECT_EQ(h->flags & NEON_IMMORTAL, 0u);
    neon_arena_drop(a);
}

TEST(distinct_allocations_do_not_overlap) {
    // Many small objects of assorted sizes, all live at once: no two may share bytes. A
    // sentinel in each payload would be corrupted by an overlap.
    enum { N = 512 };
    neon_arena* a = neon_arena_create();
    neon_header* hs[N];
    for (int i = 0; i < N; i++) {
        hs[i] = (neon_header*)neon_arena_alloc(a, (size_t)(i % 200), no_drop);
        char* payload = (char*)(hs[i] + 1);
        if (i % 200 > 0) {
            payload[0] = (char)i;
        }
    }
    for (int i = 0; i < N; i++) {
        char* payload = (char*)(hs[i] + 1);
        if (i % 200 > 0) {
            EXPECT_EQ(payload[0], (char)i);
        }
    }
    neon_arena_drop(a);
}

TEST(a_freed_slot_is_reused) {
    // Free one, allocate the same class again: the arena hands the slot back (from the
    // class free list) rather than bumping into fresh space.
    neon_arena* a = neon_arena_create();
    neon_header* x = (neon_header*)neon_arena_alloc(a, 8, no_drop);
    neon_arena_free(a, x);
    neon_header* y = (neon_header*)neon_arena_alloc(a, 8, no_drop);
    EXPECT_EQ((void*)x, (void*)y); // LIFO free list returns the last freed slot
    EXPECT_EQ(y->rc, 1u);          // a fresh object, re-initialised
    neon_arena_drop(a);
}

TEST(a_big_allocation_round_trips) {
    // Past the 512-byte class ceiling: served by malloc, tracked on the big list, usable
    // end to end, and freed straight back without disturbing the footprint accounting.
    neon_arena* a = neon_arena_create();
    neon_header* h = (neon_header*)neon_arena_alloc(a, 4096, no_drop);
    EXPECT_EQ(h->rc, 1u);
    EXPECT(h->flags & NEON_ALLOC_BIG);
    char* payload = (char*)(h + 1);
    payload[0] = 'x';
    payload[4095] = 'y';
    EXPECT_EQ(payload[0], 'x');
    EXPECT_EQ(payload[4095], 'y');
    size_t before = neon_arena_footprint(a);
    neon_arena_free(a, h);
    EXPECT(neon_arena_footprint(a) < before); // the big block left the footprint
    neon_arena_drop(a);
}

TEST(footprint_plateaus_under_churn) {
    // The load-bearing property: allocate-then-free the same shapes over and over, and the
    // footprint stops growing once each class has peaked. A pure bump allocator would climb
    // without bound here; the class free lists mean the second round reuses the first's
    // slots. We let it settle for a round, record the footprint, then churn hard and assert
    // it never rises above the settled mark.
    neon_arena* a = neon_arena_create();

    // One settling round of the exact pattern the churn will repeat.
    for (int i = 0; i < 40; i++) {
        neon_header* h = (neon_header*)neon_arena_alloc(a, (size_t)(i * 13), no_drop);
        neon_arena_free(a, h);
    }
    size_t settled = neon_arena_footprint(a);
    EXPECT(settled > 0); // it did take memory

    for (int round = 0; round < 1000; round++) {
        neon_header* hs[40];
        for (int i = 0; i < 40; i++) {
            hs[i] = (neon_header*)neon_arena_alloc(a, (size_t)(i * 13), no_drop);
        }
        for (int i = 0; i < 40; i++) {
            neon_arena_free(a, hs[i]);
        }
    }
    // Peak-live per class is identical every round (all 40 live at once, same sizes), so
    // the footprint after the settling round already covers the peak: churn adds nothing.
    EXPECT_EQ(neon_arena_footprint(a), settled);
    neon_arena_drop(a);
}

TEST(churn_across_classes_stays_consistent) {
    // Alloc/free churn across every class with a written-then-verified payload: the pattern
    // that exposes a slot returned to the wrong class list (handed back out at the wrong
    // size, so the sentinel readback fails or ASan trips).
    neon_arena* a = neon_arena_create();
    for (int round = 0; round < 100; round++) {
        neon_header* hs[40];
        for (int i = 0; i < 40; i++) {
            size_t sz = (size_t)(i * 13);
            hs[i] = (neon_header*)neon_arena_alloc(a, sz, no_drop);
            char* p = (char*)(hs[i] + 1);
            for (size_t b = 0; b < sz; b++) {
                p[b] = (char)(i + b);
            }
        }
        for (int i = 0; i < 40; i++) {
            size_t sz = (size_t)(i * 13);
            char* p = (char*)(hs[i] + 1);
            for (size_t b = 0; b < sz; b++) {
                EXPECT_EQ(p[b], (char)(i + b));
            }
            neon_arena_free(a, hs[i]);
        }
    }
    neon_arena_drop(a);
}

TEST(drop_frees_a_live_arena) {
    // No frees at all before drop — many live objects, small and big, across many chunks.
    // The bulk-free must release every chunk and every big allocation; LeakSanitizer is the
    // oracle. (This is the shape slice 1's teardown handles: objects with no outgoing
    // references, freed en masse at the fiber's death.)
    neon_arena* a = neon_arena_create();
    for (int i = 0; i < 2000; i++) {
        neon_arena_alloc(a, (size_t)(i % 700), no_drop); // mixes classes and bigs, no frees
    }
    neon_arena_drop(a);
    EXPECT(true); // reaching here leak-clean under LSan is the assertion
}

TEST(an_empty_arena_drops_clean) {
    // Created and never used: the first chunk is lazy, so this must free only the control
    // struct and take no chunk.
    neon_arena* a = neon_arena_create();
    EXPECT_EQ(neon_arena_footprint(a), (size_t)0);
    neon_arena_drop(a);
}
