// `runtime/src/resource.c`: a linear resource — a payload plus a cleanup that must run
// exactly once, when the resource is dropped still armed. `take`/`disarm` move the payload
// out and defuse the cleanup; `get` copies it and leaves the resource armed. All of
// `get`/`disarm`/`is_live`/`cleanup` consume the resource; `take` borrows it.
//
// The instantiation's `drop` is codegen-emitted in a real program: run the cleanup if still
// armed, then `neon_resource_finish`. `armed_drop` below is that shape, written by hand so
// the cleanup-exactly-once behaviour is observable.

#include "tinyunit.h"

#include "support.h"

TEST_SUITE("resource");

static int cleanup_count = 0;

// The cleanup closure's function: a `(payload) -> unit`. Increments a counter so the test
// can see it ran (or did not).
static void cleanup_fn(neon_header* env, int64_t payload) {
    (void)env;
    (void)payload;
    cleanup_count++;
}

// The per-instantiation drop, mirroring codegen: if the resource is still armed, take the
// payload out and run the cleanup on it, then land in the shared finish.
static void armed_drop(void* p) {
    neon_resource* r = (neon_resource*)p;
    int64_t payload;
    if (neon_resource_take(r, &payload)) {
        neon_closure c = r->cleanup;
        ((void (*)(neon_header*, int64_t))c.fn)(c.env, payload);
    }
    neon_resource_finish(r);
}

static neon_resource* make(int64_t payload) {
    neon_closure c = {(void*)cleanup_fn, NULL};
    return neon_resource_new(&payload, &nt_i64_w, c, armed_drop);
}

TEST(get_hands_back_the_payload_and_stays_armed) {
    cleanup_count = 0;
    neon_resource* r = make(42);
    int64_t out = 0;
    // get copies the payload and consumes r; r was still armed, so its drop runs cleanup.
    EXPECT(neon_resource_get(r, &out));
    EXPECT_EQ(out, 42);
    EXPECT_EQ(cleanup_count, 1);
}

TEST(take_moves_the_payload_and_defuses_cleanup) {
    cleanup_count = 0;
    neon_resource* r = make(7);
    int64_t out = 0;
    EXPECT(neon_resource_take(r, &out)); // borrows r
    EXPECT_EQ(out, 7);
    // A second take finds it disarmed.
    int64_t again = -1;
    EXPECT(!neon_resource_take(r, &again));
    // Releasing the (disarmed) resource must NOT run cleanup: the payload was moved out.
    neon_release((neon_header*)r);
    EXPECT_EQ(cleanup_count, 0);
}

TEST(cleanup_runs_exactly_once_at_drop) {
    cleanup_count = 0;
    neon_resource* r = make(1);
    neon_release((neon_header*)r); // dropped while armed: cleanup runs once
    EXPECT_EQ(cleanup_count, 1);
}

TEST(disarm_picks_exactly_one_winner) {
    cleanup_count = 0;
    neon_resource* r = make(99);
    neon_retain((neon_header*)r); // two references race to disarm

    int64_t first = 0, second = 0;
    bool won_first = neon_resource_disarm(r, &first);   // takes, then releases r
    bool won_second = neon_resource_disarm(r, &second); // finds it disarmed, releases r

    EXPECT(won_first);
    EXPECT_EQ(first, 99);
    EXPECT(!won_second); // exactly one winner
    EXPECT_EQ(cleanup_count, 0); // disarmed, so the drop does not clean up
}

TEST(is_live_reports_armed_state) {
    cleanup_count = 0;
    neon_resource* r = make(3);
    neon_retain((neon_header*)r);
    EXPECT(neon_resource_is_live(r)); // armed; consumes one reference
    int64_t out = 0;
    neon_resource_take(r, &out);
    EXPECT(!neon_resource_is_live(r)); // disarmed; consumes the last reference
}
