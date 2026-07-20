// `runtime/src/lifecycle.c`: the reference count every heap object rides on. `neon_alloc`
// starts an object at rc 1; retain increments, release decrements and runs `drop` at zero.
// The immortal flag and a NULL pointer make retain/release no-ops.

#include <minunit/minunit.h>

#include "support.h"

TEST_SUITE(lifecycle_suite);

namespace {
int drop_count = 0;
void counting_drop(void* p) {
    drop_count++;
    neon_free(p);
}
} // namespace

TEST(alloc_starts_at_one) {
    drop_count = 0;
    neon_header* h = (neon_header*)neon_alloc(0, counting_drop);
    TEST_EXPECT(h->rc == 1);
    TEST_EXPECT((h->flags & NEON_IMMORTAL) == 0);
    neon_release(h);
    TEST_EXPECT(drop_count == 1);
}

TEST(retain_and_release_track_the_count) {
    drop_count = 0;
    neon_header* h = (neon_header*)neon_alloc(16, counting_drop);
    neon_retain(h);
    neon_retain(h);
    TEST_EXPECT(h->rc == 3);
    neon_release(h);
    TEST_EXPECT(h->rc == 2);
    TEST_EXPECT(drop_count == 0); // not dropped while references remain
    neon_release(h);
    neon_release(h);
    TEST_EXPECT(drop_count == 1); // dropped exactly once, at zero
}

TEST(immortal_never_drops) {
    drop_count = 0;
    neon_header* h = (neon_header*)neon_alloc(0, counting_drop);
    h->flags |= NEON_IMMORTAL;
    for (int i = 0; i < 100; i++) {
        neon_retain(h);
        neon_release(h);
    }
    TEST_EXPECT(drop_count == 0);
    TEST_EXPECT(h->rc == 1); // untouched
    neon_free(h);            // an immortal object is freed by hand, not by release
}

TEST(null_is_a_noop) {
    // Neither traps; both simply return.
    neon_retain(nullptr);
    neon_release(nullptr);
    TEST_EXPECT(true);
}
