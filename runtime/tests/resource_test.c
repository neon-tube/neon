// `runtime/src/resource.c`: an owned identity — a shared BODY (payload + cleanup + armed +
// owner) and per-holder REFS, exactly one of which is OWNING; the owning ref's death runs
// cleanup. Crossing a fiber boundary mints a ref; inside a transfer bracket a copy of the
// owning ref moves the baton. `get`/`disarm` are gated: NEON_RESOURCE_OK only from the
// owner context, NEON_RESOURCE_RELEASED after cleanup ran, NEON_RESOURCE_NOT_OWNER from
// anywhere else (including while the baton is in flight).
//
// The instantiation's ref-drop is codegen-emitted in a real program: if this ref is owning
// and the body still armed, take the payload and run cleanup, then land in the shared
// finish. `owning_drop` below is that shape, written by hand so cleanup-exactly-once is
// observable. These tests run on the ROOT context, which the gate treats as a context like
// any fiber (the sentinel identity) — NOT_OWNER is observable here exactly while a baton
// is in flight (body unowned, ref non-owning).

#include "tinyunit.h"

#include "support.h"

TEST_SUITE("resource");

static int cleanup_count = 0;

static void cleanup_fn(neon_header* env, int64_t payload) {
    (void)env;
    (void)payload;
    cleanup_count++;
}

// The per-instantiation REF drop, mirroring codegen.
static void owning_drop(void* p) {
    neon_resource* r = (neon_resource*)p;
    int64_t payload;
    if (r->owning && neon_resource_take(r->body, &payload)) {
        neon_closure c = r->body->cleanup;
        ((void (*)(neon_header*, int64_t))c.fn)(c.env, payload);
    }
    neon_resource_ref_finish(r);
}

static neon_resource* make(int64_t payload) {
    neon_closure c = {(void*)cleanup_fn, NULL};
    return neon_resource_new(&payload, &nt_i64_w, c, owning_drop);
}

// Mint a second ref the way a crossing does, with or without the transfer bracket.
static neon_resource* cross(neon_resource** src, bool transfer) {
    neon_resource* out = NULL;
    if (transfer) {
        neon_transfer_begin();
    }
    neon_wcopy_resource(src, &out);
    if (transfer) {
        neon_transfer_end();
    }
    return out;
}

static bool env_dropped = false;
static void env_drop(void* p) {
    env_dropped = true;
    neon_free(p);
}

TEST(get_hands_back_the_payload_and_stays_armed) {
    cleanup_count = 0;
    neon_resource* r = make(42);
    int64_t out = 0;
    // get copies the payload and consumes r; r was the owning ref and the body was still
    // armed, so its drop runs cleanup.
    EXPECT_EQ(neon_resource_get(r, &out), NEON_RESOURCE_OK);
    EXPECT_EQ(out, 42);
    EXPECT_EQ(cleanup_count, 1);
}

TEST(get_after_disarm_reports_released) {
    cleanup_count = 0;
    neon_resource* r = make(7);
    neon_retain((neon_header*)r);
    int64_t out = 0;
    EXPECT_EQ(neon_resource_disarm(r, &out), NEON_RESOURCE_OK);
    EXPECT_EQ(out, 7);
    int64_t again = -1;
    EXPECT_EQ(neon_resource_get(r, &again), NEON_RESOURCE_RELEASED);
    EXPECT_EQ(cleanup_count, 0); // payload was taken; the drop must not clean up
}

TEST(cleanup_runs_exactly_once_at_owning_drop) {
    cleanup_count = 0;
    neon_resource* r = make(1);
    neon_release((neon_header*)r); // owning ref dropped while armed: cleanup runs once
    EXPECT_EQ(cleanup_count, 1);
}

TEST(disarm_picks_exactly_one_winner) {
    cleanup_count = 0;
    neon_resource* r = make(99);
    neon_retain((neon_header*)r);

    int64_t first = 0, second = 0;
    EXPECT_EQ(neon_resource_disarm(r, &first), NEON_RESOURCE_OK);
    EXPECT_EQ(first, 99);
    EXPECT_EQ(neon_resource_disarm(r, &second), NEON_RESOURCE_RELEASED);
    EXPECT_EQ(cleanup_count, 0);
}

TEST(is_live_reports_armed_state) {
    cleanup_count = 0;
    neon_resource* r = make(3);
    neon_retain((neon_header*)r);
    EXPECT(neon_resource_is_live(r)); // armed; consumes one reference
    int64_t out = 0;
    neon_resource_disarm(r, &out);
    // A fresh handle to observe: is_live consumed nothing we still hold.
    cleanup_count = 0;
    neon_resource* r2 = make(4);
    neon_retain((neon_header*)r2);
    int64_t o2 = 0;
    neon_resource_disarm(r2, &o2);
    EXPECT(!neon_resource_is_live(r2)); // disarmed; consumes the last reference
}

// ---- the baton ----

TEST(a_plain_crossing_mints_a_non_owning_ref_and_the_baton_stays) {
    cleanup_count = 0;
    neon_resource* r = make(5);
    neon_resource* other = cross(&r, false);
    EXPECT(r->owning);
    EXPECT(!other->owning);
    EXPECT(r->body == other->body);
    // Both usable HERE — the gate is per-context, not per-ref, and this context owns.
    int64_t out = 0;
    neon_retain((neon_header*)r);
    EXPECT_EQ(neon_resource_get(r, &out), NEON_RESOURCE_OK);
    neon_retain((neon_header*)other);
    EXPECT_EQ(neon_resource_get(other, &out), NEON_RESOURCE_OK);
    // Dropping the NON-owning ref first must not clean up; the owning ref's drop does.
    neon_release((neon_header*)other);
    EXPECT_EQ(cleanup_count, 0);
    neon_release((neon_header*)r);
    EXPECT_EQ(cleanup_count, 1);
}

TEST(a_transfer_moves_the_baton_and_strands_the_source) {
    cleanup_count = 0;
    neon_resource* r = make(6);
    neon_resource* moved = cross(&r, true);
    EXPECT(!r->owning); // demoted
    EXPECT(moved->owning);
    EXPECT(moved->body->owner == NULL); // in flight until first use claims
    // The stranded source alias: body unowned, ref non-owning -> NOT_OWNER.
    int64_t out = 0;
    neon_retain((neon_header*)r);
    EXPECT_EQ(neon_resource_get(r, &out), NEON_RESOURCE_NOT_OWNER);
    // First use through the owning ref claims for this context.
    neon_retain((neon_header*)moved);
    EXPECT_EQ(neon_resource_get(moved, &out), NEON_RESOURCE_OK);
    EXPECT_EQ(out, 6);
    EXPECT(moved->body->owner != NULL);
    // Cleanup rides the owning ref, exactly once, in either drop order.
    neon_release((neon_header*)r);
    EXPECT_EQ(cleanup_count, 0);
    neon_release((neon_header*)moved);
    EXPECT_EQ(cleanup_count, 1);
}

TEST(an_unused_moved_ref_still_cleans_up_on_drop) {
    cleanup_count = 0;
    neon_resource* r = make(8);
    neon_resource* moved = cross(&r, true);
    neon_release((neon_header*)r);
    EXPECT_EQ(cleanup_count, 0);
    // The dead-letter shape: the owning ref is dropped without ever being used (a channel
    // discarding an undelivered resource). Cleanup must still run — the baton needs no
    // claim to be owed.
    neon_release((neon_header*)moved);
    EXPECT_EQ(cleanup_count, 1);
}

TEST(a_transfer_from_a_non_owning_ref_moves_nothing) {
    cleanup_count = 0;
    neon_resource* r = make(9);
    neon_resource* named = cross(&r, false); // a non-owning name
    neon_resource* second = cross(&named, true); // "sending" the name
    EXPECT(r->owning);
    EXPECT(!named->owning);
    EXPECT(!second->owning);
    EXPECT(r->body->owner != NULL); // baton never went in flight
    neon_release((neon_header*)named);
    neon_release((neon_header*)second);
    EXPECT_EQ(cleanup_count, 0);
    neon_release((neon_header*)r);
    EXPECT_EQ(cleanup_count, 1);
}

TEST(cleanup_hands_back_the_closure_and_retains_its_env) {
    cleanup_count = 0;
    env_dropped = false;

    neon_header* env = (neon_header*)neon_alloc(0, env_drop); // rc == 1
    neon_closure c = {(void*)cleanup_fn, env};
    int64_t payload = 5;
    neon_resource* r = neon_resource_new(&payload, &nt_i64_w, c, owning_drop);

    // `cleanup` retains `env`, then consumes `r`. `r` is the last (owning) ref, so its
    // drop runs (armed -> cleanup_fn once), the body dies with it, and the body's drop
    // releases the resource's own `env` reference. The reference `cleanup` retained keeps
    // `env` alive across that.
    neon_closure got = neon_resource_cleanup(r);
    EXPECT_EQ(got.fn, (void*)cleanup_fn);
    EXPECT(got.env == env);
    EXPECT(!env_dropped);
    EXPECT_EQ(cleanup_count, 1);

    neon_release(got.env);
    EXPECT(env_dropped);
}
