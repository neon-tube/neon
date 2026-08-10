// The teardown walk (docs/design/fibers.md): when a fiber dies — crash or exit — every
// object still live in its arena has its drop forced, so a Resource runs its cleanup and
// references OUT of the arena (to shared/slab objects) are released, before the bulk-free.
// Teardown mode makes the forced drops exactly-once: arena-internal releases are no-ops.
// LSan is the standing oracle that the bulk-free then leaks nothing.

#include "tinyunit.h"

#include "support.h"

#include "neon/fiber.h"

TEST_SUITE("fiber_teardown");

// ---- a crashed fiber's Resource runs its cleanup ----

static int td_cleanup_count;
static void td_cleanup_fn(neon_header* env, int64_t payload) {
    (void)env;
    (void)payload;
    td_cleanup_count++;
}
static void td_armed_drop(void* p) {
    neon_resource* r = (neon_resource*)p;
    int64_t payload;
    if (neon_resource_take(r, &payload)) {
        neon_closure c = r->cleanup;
        ((void (*)(neon_header*, int64_t))c.fn)(c.env, payload);
    }
    neon_resource_finish(r);
}

static void resource_then_crash(void* arg) {
    (void)arg;
    int64_t payload = 7;
    neon_closure c = {(void*)td_cleanup_fn, NULL};
    neon_resource_new(&payload, &nt_i64_w, c, td_armed_drop); // held by this frame only
    neon_trap("crash with an armed resource (test)");
}

TEST(a_crashed_fibers_resource_cleanup_runs) {
    td_cleanup_count = 0;
    neon_fiber_runtime(resource_then_crash, NULL);
    // The fiber died at the trap; nothing released the resource — the WALK found it live in
    // the arena and its forced drop ran the cleanup exactly once.
    EXPECT_EQ(td_cleanup_count, 1);
}

static void resource_then_leak(void* arg) {
    (void)arg;
    int64_t payload = 9;
    neon_closure c = {(void*)td_cleanup_fn, NULL};
    neon_resource_new(&payload, &nt_i64_w, c, td_armed_drop);
    // No crash: the body just returns without releasing — the walk covers normal exit too.
}

TEST(a_leaked_resource_cleanup_runs_at_fiber_exit) {
    td_cleanup_count = 0;
    neon_fiber_runtime(resource_then_leak, NULL);
    EXPECT_EQ(td_cleanup_count, 1);
}

// ---- an outgoing reference held in arena DATA is released exactly once ----

// A shared (slab) object created off-fiber, with a counting drop.
static int td_shared_drops;
static void td_shared_drop(void* p) {
    td_shared_drops++;
    neon_free(p);
}

// An arena wrapper holding one reference to the shared object; its drop releases it — the
// per-type knowledge the walk relies on.
typedef struct {
    neon_header header;
    neon_header* shared;
} td_holder;

static void td_holder_drop(void* p) {
    td_holder* h = (td_holder*)p;
    neon_release(h->shared);
    neon_free(h);
}

static neon_header* td_shared; // created by the test on the root context (slab)

static void hold_shared_then_crash(void* arg) {
    (void)arg;
    // The wrapper lives in THIS fiber's arena and owns one reference to the slab object.
    td_holder* h = (td_holder*)neon_alloc(sizeof(td_holder) - sizeof(neon_header), td_holder_drop);
    h->shared = td_shared;
    neon_retain(td_shared);
    neon_trap("crash holding a shared reference (test)");
}

TEST(a_crashed_fibers_outgoing_refs_are_released) {
    td_shared_drops = 0;
    td_shared = (neon_header*)neon_alloc(0, td_shared_drop); // root context: slab, rc 1
    neon_fiber_runtime(hold_shared_then_crash, NULL);
    // The wrapper died with the fiber; the walk's forced drop released its ONE outgoing
    // reference, so our own release below is the last and the shared object drops once.
    EXPECT_EQ(td_shared_drops, 0);
    EXPECT_EQ(td_shared->rc, 1u);
    neon_release(td_shared);
    EXPECT_EQ(td_shared_drops, 1);
}

// ---- internal references stay exactly-once under the walk ----

// Two arena objects, A holding a reference to B (B rc 2: A + the abandoned frame). The walk
// force-drops both in address order; teardown mode's no-op on internal releases is what
// stops A's drop from double-releasing an already-freed B (or vice versa). ASan would flag
// any double handling; the assertion here is simply surviving with the drops each run once.
static int td_pair_drops;
typedef struct {
    neon_header header;
    neon_header* other; // arena-internal reference (or NULL)
} td_node;

static void td_node_drop(void* p) {
    td_node* n = (td_node*)p;
    td_pair_drops++;
    neon_release(n->other); // internal: a no-op under teardown
    neon_free(n);
}

static void internal_pair_then_crash(void* arg) {
    (void)arg;
    td_node* b = (td_node*)neon_alloc(sizeof(td_node) - sizeof(neon_header), td_node_drop);
    b->other = NULL;
    td_node* a = (td_node*)neon_alloc(sizeof(td_node) - sizeof(neon_header), td_node_drop);
    a->other = (neon_header*)b;
    neon_retain((neon_header*)b); // A's reference
    neon_trap("crash with internal structure (test)");
}

TEST(internal_references_do_not_double_drop) {
    td_pair_drops = 0;
    neon_fiber_runtime(internal_pair_then_crash, NULL);
    EXPECT_EQ(td_pair_drops, 2); // each node force-dropped exactly once
}
