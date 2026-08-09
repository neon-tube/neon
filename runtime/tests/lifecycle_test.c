// `runtime/src/lifecycle.c`: the reference count every heap object rides on. `neon_alloc`
// starts an object at rc 1; retain increments, release decrements and runs `drop` at zero.
// The immortal flag and a NULL pointer make retain/release no-ops.

#include "tinyunit.h"

#include "support.h"

TEST_SUITE("lifecycle");

static int drop_count = 0;
static void counting_drop(void* p) {
    drop_count++;
    neon_free(p);
}

TEST(alloc_starts_at_one) {
    drop_count = 0;
    neon_header* h = (neon_header*)neon_alloc(0, counting_drop);
    EXPECT_EQ(h->rc, 1u);
    EXPECT_EQ(h->flags & NEON_IMMORTAL, 0u);
    neon_release(h);
    EXPECT_EQ(drop_count, 1);
}

TEST(retain_and_release_track_the_count) {
    drop_count = 0;
    neon_header* h = (neon_header*)neon_alloc(16, counting_drop);
    neon_retain(h);
    neon_retain(h);
    EXPECT_EQ(h->rc, 3u);
    neon_release(h);
    EXPECT_EQ(h->rc, 2u);
    EXPECT_EQ(drop_count, 0); // not dropped while references remain
    neon_release(h);
    neon_release(h);
    EXPECT_EQ(drop_count, 1); // dropped exactly once, at zero
}

TEST(immortal_never_drops) {
    drop_count = 0;
    neon_header* h = (neon_header*)neon_alloc(0, counting_drop);
    h->flags |= NEON_IMMORTAL;
    for (int i = 0; i < 100; i++) {
        neon_retain(h);
        neon_release(h);
    }
    EXPECT_EQ(drop_count, 0);
    EXPECT_EQ(h->rc, 1u); // untouched
    neon_free(h);         // an immortal object is freed by hand, not by release
}

TEST(null_is_a_noop) {
    // Neither traps; both simply return.
    neon_retain(NULL);
    neon_release(NULL);
    EXPECT(true);
}

// ---- the slab allocator ----
//
// Behind `neon_alloc` is a size-class free-list slab (see src/lifecycle.c). These check
// what the slab must not get wrong: distinct live objects never overlap, a freed block
// comes back for reuse, the class survives a round trip, and a block too big for any
// class still works. ASan is the real integrity oracle across the whole corpus; these
// pin the specific behaviours a slab bug would break.

TEST(distinct_allocations_do_not_overlap) {
    // Many small objects of assorted sizes, all live at once: no two may share bytes.
    enum { N = 256 };
    neon_header* hs[N];
    for (int i = 0; i < N; i++) {
        hs[i] = (neon_header*)neon_alloc((size_t)(i % 200), counting_drop);
        // Write a sentinel into the payload so an overlap would corrupt a neighbour.
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
    for (int i = 0; i < N; i++) {
        neon_release(hs[i]);
    }
    EXPECT_EQ(drop_count, N);
}

TEST(a_freed_block_is_reused) {
    drop_count = 0;
    // Free one, allocate the same class again: the slab should hand the block back
    // rather than growing.
    neon_header* a = (neon_header*)neon_alloc(8, counting_drop);
    neon_release(a);
    neon_header* b = (neon_header*)neon_alloc(8, counting_drop);
    EXPECT_EQ((void*)a, (void*)b); // LIFO free list returns the last freed block
    EXPECT_EQ(b->rc, 1u);          // and it is a fresh object, not the released one
    neon_release(b);
}

TEST(a_large_allocation_bypasses_the_slab) {
    // Past the slab's 512-byte cap: served by malloc, marked, and freed straight back.
    drop_count = 0;
    neon_header* h = (neon_header*)neon_alloc(4096, counting_drop);
    EXPECT_EQ(h->rc, 1u);
    char* payload = (char*)(h + 1);
    payload[0] = 'x';
    payload[4095] = 'y';
    EXPECT_EQ(payload[0], 'x');
    EXPECT_EQ(payload[4095], 'y');
    neon_release(h);
    EXPECT_EQ(drop_count, 1);
}

TEST(churn_across_classes_stays_consistent) {
    // Alloc/free churn across every size class, interleaved: the pattern that would
    // expose a class mixed up on free (a block returned to the wrong list, handed out
    // at the wrong size).
    for (int round = 0; round < 100; round++) {
        neon_header* hs[40];
        for (int i = 0; i < 40; i++) {
            size_t sz = (size_t)(i * 13);
            hs[i] = (neon_header*)neon_alloc(sz, counting_drop);
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
            neon_release(hs[i]);
        }
    }
    EXPECT(true);
}
