// `runtime/src/fiber.c` + the swap gadget: stackful cooperative fibers (slice 2 of
// docs/design/fibers.md). These drive control back and forth across raw stack swaps and
// assert it lands where it should — and, run under AddressSanitizer in this suite, that the
// fiber annotations keep ASan's fake-stack tracking honest across the swap. A missing or
// mis-ordered annotation shows up here as a phantom stack-use-after-return, not as a wrong
// count, which is exactly why these run instrumented.

#include "tinyunit.h"

#include "support.h"

#include "neon/fiber.h"

TEST_SUITE("fiber");

TEST(current_is_stable_on_a_thread) {
    neon_fiber* a = neon_fiber_current();
    neon_fiber* b = neon_fiber_current();
    EXPECT(a != NULL);
    EXPECT_EQ(a, b); // the adopted root fiber is the same handle every time
}

static bool ran;
static void set_ran(void* arg) {
    (void)arg;
    ran = true;
}

TEST(a_fiber_runs_and_returns_to_main) {
    ran = false;
    neon_fiber* f = neon_fiber_new(set_ran, NULL, 0);
    EXPECT(!neon_fiber_finished(f));
    neon_fiber_resume(f); // runs the body to completion, then control returns here
    EXPECT(ran);
    EXPECT(neon_fiber_finished(f));
    neon_fiber_free(f);
}

static void add_41(void* arg) {
    int* p = (int*)arg;
    *p += 41;
}

TEST(the_arg_reaches_the_body) {
    int x = 1;
    neon_fiber* f = neon_fiber_new(add_41, &x, 0);
    neon_fiber_resume(f);
    EXPECT_EQ(x, 42);
    neon_fiber_free(f);
}

static int pingpong_turns;
static void pingpong_body(void* arg) {
    (void)arg;
    for (int i = 0; i < 5; i++) {
        pingpong_turns++;
        neon_fiber_yield(); // hand control back to whoever resumed us, five times
    }
}

TEST(resume_and_yield_alternate) {
    pingpong_turns = 0;
    neon_fiber* f = neon_fiber_new(pingpong_body, NULL, 0);
    for (int i = 0; i < 5; i++) {
        EXPECT(!neon_fiber_finished(f)); // suspended at a yield, not done
        neon_fiber_resume(f);            // runs one loop iteration, up to the next yield
        EXPECT_EQ(pingpong_turns, i + 1);
    }
    EXPECT(!neon_fiber_finished(f)); // parked at the fifth yield; the loop hasn't exited yet
    neon_fiber_resume(f);            // resumes past the last yield, body returns
    EXPECT(neon_fiber_finished(f));
    neon_fiber_free(f);
}

// The resume-link is a chain: main -> outer -> inner, and each finish unwinds one hop back.
static int nested_log[3];
static int nested_i;
static neon_fiber* inner_f;
static void inner_body(void* arg) {
    (void)arg;
    nested_log[nested_i++] = 20;
}
static void outer_body(void* arg) {
    (void)arg;
    nested_log[nested_i++] = 10;
    neon_fiber_resume(inner_f);    // outer resumes inner; returns here when inner finishes
    nested_log[nested_i++] = 11;
}

TEST(a_fiber_can_resume_another) {
    nested_i = 0;
    inner_f = neon_fiber_new(inner_body, NULL, 0);
    neon_fiber* outer = neon_fiber_new(outer_body, NULL, 0);
    neon_fiber_resume(outer);
    EXPECT_EQ(nested_i, 3);
    EXPECT_EQ(nested_log[0], 10); // outer starts
    EXPECT_EQ(nested_log[1], 20); // inner runs to completion
    EXPECT_EQ(nested_log[2], 11); // control lands back in outer, which then finishes to main
    neon_fiber_free(inner_f);
    neon_fiber_free(outer);
}

// Each fiber has its own stack: deep recursion in one must not touch main's or another's.
static uint64_t deep_result;
static uint64_t deep_sum(int n) {
    volatile char pad[256]; // force a real frame so the recursion actually uses the stack
    pad[0] = (char)n;
    pad[255] = 1;
    if (n == 0) {
        return (uint64_t)(pad[255] - 1); // 0, but through the padded frame
    }
    return (uint64_t)n + deep_sum(n - 1);
}
static void deep_body(void* arg) {
    deep_result = deep_sum(*(int*)arg);
}

TEST(a_fiber_has_its_own_deep_stack) {
    int n = 100;
    // ~100 frames * (256 bytes + ASan redzones): give it room rather than the 64K floor.
    neon_fiber* f = neon_fiber_new(deep_body, &n, 256u * 1024u);
    neon_fiber_resume(f);
    EXPECT(neon_fiber_finished(f));
    EXPECT_EQ(deep_result, (uint64_t)(100 * 101 / 2)); // 5050
    neon_fiber_free(f);
}

// Three fibers resumed out of creation order: each runs exactly once, in resume order.
static int order[3];
static int order_n;
static void record(void* arg) {
    order[order_n++] = *(int*)arg;
}

TEST(several_fibers_each_run_once) {
    order_n = 0;
    int a = 1, b = 2, c = 3;
    neon_fiber* fa = neon_fiber_new(record, &a, 0);
    neon_fiber* fb = neon_fiber_new(record, &b, 0);
    neon_fiber* fc = neon_fiber_new(record, &c, 0);
    neon_fiber_resume(fb);
    neon_fiber_resume(fa);
    neon_fiber_resume(fc);
    EXPECT_EQ(order_n, 3);
    EXPECT_EQ(order[0], 2);
    EXPECT_EQ(order[1], 1);
    EXPECT_EQ(order[2], 3);
    neon_fiber_free(fa);
    neon_fiber_free(fb);
    neon_fiber_free(fc);
}

// Create / run-a-big-frame / free, many rounds. malloc reuses the freed stacks, so this is
// where the *exit* annotation earns its keep: when a fiber finishes, the switch away passes a
// NULL fake-stack-save, telling ASan to DISCARD that fiber's fake stack. Drop that annotation
// and, with detect_stack_use_after_return on, the fake stacks and the redzone poison around
// `buf` outlive the freed stack and trip a later round on the reused memory. With it, clean.
static uint64_t churn_sink;
static void big_frame_body(void* arg) {
    (void)arg;
    volatile char buf[4096]; // ASan wraps this in poisoned redzones
    for (int i = 0; i < 4096; i += 64) {
        buf[i] = (char)i;
    }
    uint64_t s = 0;
    for (int i = 0; i < 4096; i += 64) {
        s += (unsigned char)buf[i];
    }
    churn_sink += s;
}

TEST(create_run_free_churn_stays_clean) {
    churn_sink = 0;
    for (int r = 0; r < 200; r++) {
        neon_fiber* f = neon_fiber_new(big_frame_body, NULL, 64u * 1024u);
        neon_fiber_resume(f);
        EXPECT(neon_fiber_finished(f));
        neon_fiber_free(f); // stack returns to malloc; the next round likely reuses it
    }
    EXPECT(churn_sink > 0);
}

TEST(a_never_resumed_fiber_frees_clean) {
    // Created, never run: neon_fiber_free must release the stack and struct (LSan is the
    // oracle). This is the drop-before-first-resume corner.
    neon_fiber* f = neon_fiber_new(set_ran, NULL, 0);
    neon_fiber_free(f);
    EXPECT(true);
}
