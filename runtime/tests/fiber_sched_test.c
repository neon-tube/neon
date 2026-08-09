// `runtime/src/fiber_sched.c` and the current-arena routing it turns on: the cooperative
// scheduler (slice 3 of docs/design/fibers.md). These drive fiber::runtime — spawn, FIFO
// ordering, cooperative round-robin yield, nested spawning — and check that allocation inside
// a fiber comes from that fiber's arena while the root context still uses the slab, and that a
// fiber's live objects are bulk-reclaimed at teardown (LSan is the oracle for the last).

#include "tinyunit.h"

#include "support.h"

#include "neon/fiber.h"

TEST_SUITE("fiber_sched");

static bool sched_ran;
static void sched_set_ran(void* arg) {
    (void)arg;
    sched_ran = true;
}

TEST(runtime_runs_the_body) {
    sched_ran = false;
    neon_fiber_runtime(sched_set_ran, NULL);
    EXPECT(sched_ran); // returns only after the whole fiber tree has finished
}

// spawn: the parent enqueues three children, which run FIFO after it finishes.
static int run_log[4];
static int run_n;
static int child_vals[3] = {1, 2, 3};
static void spawn_child(void* arg) {
    run_log[run_n++] = *(int*)arg;
}
static void spawn_parent(void* arg) {
    (void)arg;
    run_log[run_n++] = 0; // parent runs first
    neon_fiber_spawn(spawn_child, &child_vals[0]);
    neon_fiber_spawn(spawn_child, &child_vals[1]);
    neon_fiber_spawn(spawn_child, &child_vals[2]);
}

TEST(spawn_runs_children_fifo) {
    run_n = 0;
    neon_fiber_runtime(spawn_parent, NULL);
    EXPECT_EQ(run_n, 4);
    EXPECT_EQ(run_log[0], 0);
    EXPECT_EQ(run_log[1], 1);
    EXPECT_EQ(run_log[2], 2);
    EXPECT_EQ(run_log[3], 3);
}

// Cooperative yield: two children each log twice with a yield between, so they interleave.
static int rr_log[4];
static int rr_n;
static int rr_ids[2] = {1, 2};
static void rr_body(void* arg) {
    int id = *(int*)arg;
    for (int i = 0; i < 2; i++) {
        rr_log[rr_n++] = id;
        neon_fiber_sched_yield();
    }
}
static void rr_parent(void* arg) {
    (void)arg;
    neon_fiber_spawn(rr_body, &rr_ids[0]);
    neon_fiber_spawn(rr_body, &rr_ids[1]);
}

TEST(cooperative_yield_round_robins) {
    rr_n = 0;
    neon_fiber_runtime(rr_parent, NULL);
    // A,B enqueued. A logs 1 yields, B logs 2 yields, A logs 1 yields, B logs 2 yields,
    // A finishes, B finishes: 1,2,1,2.
    EXPECT_EQ(rr_n, 4);
    EXPECT_EQ(rr_log[0], 1);
    EXPECT_EQ(rr_log[1], 2);
    EXPECT_EQ(rr_log[2], 1);
    EXPECT_EQ(rr_log[3], 2);
}

// A child spawns a grandchild: spawning is not limited to the first fiber.
static int gen_log[3];
static int gen_n;
static void grandchild(void* arg) {
    (void)arg;
    gen_log[gen_n++] = 3;
}
static void child_spawns(void* arg) {
    (void)arg;
    gen_log[gen_n++] = 2;
    neon_fiber_spawn(grandchild, NULL);
}
static void root_spawns(void* arg) {
    (void)arg;
    gen_log[gen_n++] = 1;
    neon_fiber_spawn(child_spawns, NULL);
}

TEST(spawning_nests) {
    gen_n = 0;
    neon_fiber_runtime(root_spawns, NULL);
    EXPECT_EQ(gen_n, 3);
    EXPECT_EQ(gen_log[0], 1);
    EXPECT_EQ(gen_log[1], 2);
    EXPECT_EQ(gen_log[2], 3);
}

// ---- arena routing (the current-arena plumbing in lifecycle.c) ----

static void plain_drop(void* p) {
    neon_free(p);
}

static uint32_t in_fiber_flags;
static void alloc_probe_body(void* arg) {
    (void)arg;
    neon_header* h = (neon_header*)neon_alloc(64, plain_drop);
    in_fiber_flags = h->flags; // captured before release so the test can inspect it
    neon_release(h);           // rc 1->0 -> drop -> neon_free -> routed back to this arena
}

TEST(alloc_inside_a_fiber_comes_from_its_arena) {
    in_fiber_flags = 0;
    neon_fiber_runtime(alloc_probe_body, NULL);
    EXPECT(in_fiber_flags & NEON_ALLOC_ARENA);      // routed to the fiber's arena
    EXPECT_EQ(in_fiber_flags & NEON_ALLOC_BIG, 0u); // small, so a class not a big block
}

TEST(alloc_off_fiber_still_uses_the_slab) {
    // On the root/main context there is no current arena, so allocation is the slab exactly
    // as before fibers existed — the non-fiber fast path.
    neon_header* h = (neon_header*)neon_alloc(64, plain_drop);
    EXPECT_EQ(h->flags & NEON_ALLOC_ARENA, 0u);
    neon_release(h);
}

// A fiber that leaves objects live: the arena bulk-drop at teardown must reclaim them.
static void leaky_body(void* arg) {
    (void)arg;
    for (int i = 0; i < 200; i++) {
        neon_alloc(48, plain_drop); // never released — live in the arena at fiber exit
    }
}

TEST(a_fibers_live_objects_are_bulk_freed) {
    // LSan is the oracle: without the arena_drop in neon_fiber_free these 200 objects leak.
    neon_fiber_runtime(leaky_body, NULL);
    EXPECT(true);
}
