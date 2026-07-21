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
