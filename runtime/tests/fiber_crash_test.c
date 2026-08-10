// Crash isolation (slice 4b of docs/design/fibers.md): a trap inside a fiber kills that
// fiber and returns control to the scheduler, rather than ending the process. The mechanism
// is the ordinary context swap — a crashing fiber switches back to its resume-link exactly as
// a finishing one does, flagged crashed — so there is no setjmp/longjmp. These drive real
// traps (neon_trap) from fibers and check the process survives, sibling fibers still run, and
// the dead fiber's stack and arena are reclaimed (LSan is the oracle for the last).
//
// Expect "neon: ... (test)" lines on stderr while these run: that is a fiber reporting its
// own crash before the scheduler reaps it, not a suite failure.

#include "tinyunit.h"

#include "support.h"

#include "neon/fiber.h"

TEST_SUITE("fiber_crash");

static int crash_log[3];
static int crash_log_n;
static int crash_vals[3] = {1, 2, 3};

static void ok_body(void* arg) {
    crash_log[crash_log_n++] = *(int*)arg;
}
static void crash_body(void* arg) {
    crash_log[crash_log_n++] = *(int*)arg;
    neon_trap("intentional fiber crash (test)"); // never returns
}
static void mixed_parent(void* arg) {
    (void)arg;
    neon_fiber_spawn(ok_body, &crash_vals[0]);    // 1
    neon_fiber_spawn(crash_body, &crash_vals[1]); // 2 logs then traps
    neon_fiber_spawn(ok_body, &crash_vals[2]);    // 3 must still run
}

TEST(a_trapping_fiber_does_not_kill_the_process) {
    crash_log_n = 0;
    neon_fiber_runtime(mixed_parent, NULL);
    // Returning here at all means the process survived fiber 2's trap. All three ran, in
    // order — the crash took out only its own fiber.
    EXPECT_EQ(crash_log_n, 3);
    EXPECT_EQ(crash_log[0], 1);
    EXPECT_EQ(crash_log[1], 2);
    EXPECT_EQ(crash_log[2], 3);
}

static void plain_drop(void* p) {
    neon_free(p);
}
static void crash_holding_objects(void* arg) {
    (void)arg;
    for (int i = 0; i < 100; i++) {
        neon_alloc(48, plain_drop); // live in the fiber's arena at the moment it crashes
    }
    neon_trap("crash holding live arena objects (test)");
}

TEST(a_crashed_fibers_arena_and_stack_are_reclaimed) {
    // The stack frames from entry to the trap are abandoned unwound; neon_fiber_free must
    // still reclaim the stack, the arena, and the 100 live objects in it. LSan is the oracle.
    neon_fiber_runtime(crash_holding_objects, NULL);
    EXPECT(true);
}

static int crashers_run;
static void always_crash(void* arg) {
    (void)arg;
    crashers_run++;
    neon_trap("crash (test)");
}
static void spawn_five_crashers(void* arg) {
    (void)arg;
    for (int i = 0; i < 5; i++) {
        neon_fiber_spawn(always_crash, NULL);
    }
}

TEST(a_queue_full_of_crashers_all_drain) {
    // Every fiber crashes; the scheduler must reap each and drain the queue, then return.
    crashers_run = 0;
    neon_fiber_runtime(spawn_five_crashers, NULL);
    EXPECT_EQ(crashers_run, 5);
}

// A crash deep in a recursive call: the entire call chain is abandoned in one switch.
static void recurse_then_crash(int n) {
    volatile char pad[128]; // a real frame per level
    pad[0] = (char)n;
    if (n == 0) {
        neon_trap("deep crash (test)");
    }
    recurse_then_crash(n - 1);
}
static void deep_crash_body(void* arg) {
    (void)arg;
    recurse_then_crash(50);
}
static bool after_deep_ran;
static void after_deep(void* arg) {
    (void)arg;
    after_deep_ran = true;
}
static void deep_parent(void* arg) {
    (void)arg;
    neon_fiber_spawn(deep_crash_body, NULL);
    neon_fiber_spawn(after_deep, NULL);
}

TEST(a_deep_stack_crash_is_isolated) {
    after_deep_ran = false;
    neon_fiber_runtime(deep_parent, NULL);
    EXPECT(after_deep_ran); // the fiber queued after the deep-crashing one still ran
}
