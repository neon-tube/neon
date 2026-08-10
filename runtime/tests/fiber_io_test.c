// Safepoint preemption and the completion-IO seam (slice 6 of docs/design/fibers.md). The
// safepoint tests are portable; the IO tests are Linux (epoll), driven through a pipe: a
// fiber that reads an empty pipe parks on the descriptor, the scheduler runs everyone else and
// waits in the kernel only when idle, and readiness wakes the reader.

#include "tinyunit.h"

#include "support.h"

#include "neon/fiber.h"

TEST_SUITE("fiber_io");

// ---- safepoint preemption ----

static int sp_ids[2] = {1, 2};
static int sp_log[6];
static int sp_n;

static void sp_worker(void* arg) {
    int id = *(int*)arg;
    for (int i = 0; i < 3; i++) {
        sp_log[sp_n++] = id;
        neon_fiber_request_preempt(); // a timer would do this in production
        neon_fiber_safepoint();       // ...and this back-edge check yields
    }
}
static void sp_parent(void* arg) {
    (void)arg;
    neon_fiber_spawn(sp_worker, &sp_ids[0]);
    neon_fiber_spawn(sp_worker, &sp_ids[1]);
}

TEST(safepoint_yields_when_preemption_is_requested) {
    sp_n = 0;
    neon_fiber_runtime(sp_parent, NULL);
    // Each worker yields at every iteration's safepoint, so they interleave: 1,2,1,2,1,2.
    EXPECT_EQ(sp_n, 6);
    EXPECT_EQ(sp_log[0], 1);
    EXPECT_EQ(sp_log[1], 2);
    EXPECT_EQ(sp_log[2], 1);
    EXPECT_EQ(sp_log[3], 2);
    EXPECT_EQ(sp_log[4], 1);
    EXPECT_EQ(sp_log[5], 2);
}

static int nosp_log[4];
static int nosp_n;
static void nosp_worker(void* arg) {
    int id = *(int*)arg;
    for (int i = 0; i < 2; i++) {
        nosp_log[nosp_n++] = id;
        neon_fiber_safepoint(); // no preemption requested → a no-op, no yield
    }
}
static void nosp_parent(void* arg) {
    (void)arg;
    neon_fiber_spawn(nosp_worker, &sp_ids[0]);
    neon_fiber_spawn(nosp_worker, &sp_ids[1]);
}

TEST(safepoint_without_a_request_does_not_yield) {
    nosp_n = 0;
    neon_fiber_runtime(nosp_parent, NULL);
    // With nothing requesting preemption each worker runs straight through: 1,1 then 2,2.
    EXPECT_EQ(nosp_n, 4);
    EXPECT_EQ(nosp_log[0], 1);
    EXPECT_EQ(nosp_log[1], 1);
    EXPECT_EQ(nosp_log[2], 2);
    EXPECT_EQ(nosp_log[3], 2);
}

// The timer end of preemption: a spinner that never yields VOLUNTARILY — it only calls the
// safepoint, as codegen would at a loop back-edge — must still be preempted by the CPU-time
// timer so a sibling fiber gets a turn. Without the timer this spins its full bound and the
// sibling never runs before it finishes; with it, the sibling's flag flips mid-spin.
static volatile bool tp_sibling_ran;
static bool tp_preempted;
static void tp_spinner(void* arg) {
    (void)arg;
    // ~a few hundred ms of spinning at worst — the 10ms timer fires long before the bound.
    for (long i = 0; i < 2000000000L && !tp_sibling_ran; i++) {
        neon_fiber_safepoint();
    }
    tp_preempted = tp_sibling_ran; // true only if the sibling ran while we were mid-loop
}
static void tp_sibling(void* arg) {
    (void)arg;
    tp_sibling_ran = true;
}
static void tp_parent(void* arg) {
    (void)arg;
    neon_fiber_spawn(tp_spinner, NULL); // queued first: runs first, yields only via safepoint
    neon_fiber_spawn(tp_sibling, NULL);
}

TEST(the_timer_preempts_a_spinning_fiber) {
    tp_sibling_ran = false;
    tp_preempted = false;
    neon_fiber_runtime(tp_parent, NULL);
    EXPECT(tp_preempted);
}

// ---- the completion-IO seam (Linux/epoll) ----

#if defined(__linux__)
#include <fcntl.h>
#include <unistd.h>

static int io_pipe[2];
static int io_got;

static void io_reader(void* arg) {
    (void)arg;
    char buf[8];
    ssize_t r = neon_fiber_read(io_pipe[0], buf, sizeof(buf)); // empty pipe → parks on EPOLLIN
    if (r > 0) {
        io_got = (unsigned char)buf[0];
    }
}
static void io_writer(void* arg) {
    (void)arg;
    char c = 77;
    ssize_t w = write(io_pipe[1], &c, 1); // makes the read end ready
    (void)w;
}
static void io_parent(void* arg) {
    (void)arg;
    neon_fiber_spawn(io_reader, NULL); // runs first, blocks on the empty pipe, parks
    neon_fiber_spawn(io_writer, NULL); // runs next, writes; the pump then epoll-wakes the reader
}

TEST(fiber_read_parks_until_data_arrives) {
    if (pipe(io_pipe) != 0) {
        EXPECT(false);
        return;
    }
    fcntl(io_pipe[0], F_SETFL, O_NONBLOCK); // the reading fiber must not block the whole thread
    io_got = 0;
    neon_fiber_runtime(io_parent, NULL);
    EXPECT_EQ(io_got, 77); // the byte the writer sent, delivered after an epoll wakeup
    close(io_pipe[0]);
    close(io_pipe[1]);
}

static void io_single_reader(void* arg) {
    (void)arg;
    char buf[4];
    ssize_t r = neon_fiber_read(io_pipe[0], buf, sizeof(buf));
    if (r > 0) {
        io_got = (unsigned char)buf[0];
    }
}

TEST(fiber_read_returns_ready_data_without_parking) {
    if (pipe(io_pipe) != 0) {
        EXPECT(false);
        return;
    }
    fcntl(io_pipe[0], F_SETFL, O_NONBLOCK);
    char c = 88;
    ssize_t w = write(io_pipe[1], &c, 1); // data already buffered before the fiber reads
    (void)w;
    io_got = 0;
    neon_fiber_runtime(io_single_reader, NULL);
    EXPECT_EQ(io_got, 88); // read succeeds immediately; no epoll wait needed
    close(io_pipe[0]);
    close(io_pipe[1]);
}
#endif

// ---- fiber sleep (the deadline list) ----

// A sleeping fiber must not block its siblings: the long sleeper is queued FIRST but
// finishes LAST, and the whole runtime takes about as long as the longest sleep — not the
// sum — because the scheduler waits in the kernel only when nothing is runnable.
static int sl_log[3];
static int sl_n;
static void sl_long(void* arg) {
    (void)arg;
    neon_fiber_sleep(60);
    sl_log[sl_n++] = 3;
}
static void sl_short(void* arg) {
    (void)arg;
    neon_fiber_sleep(20);
    sl_log[sl_n++] = 2;
}
static void sl_instant(void* arg) {
    (void)arg;
    sl_log[sl_n++] = 1;
}
static void sl_parent(void* arg) {
    (void)arg;
    neon_fiber_spawn(sl_long, NULL);    // queued first, finishes last
    neon_fiber_spawn(sl_short, NULL);
    neon_fiber_spawn(sl_instant, NULL); // no sleep, finishes first
}

TEST(sleeping_fibers_wake_in_deadline_order) {
    sl_n = 0;
    neon_fiber_runtime(sl_parent, NULL);
    EXPECT_EQ(sl_n, 3);
    EXPECT_EQ(sl_log[0], 1);
    EXPECT_EQ(sl_log[1], 2);
    EXPECT_EQ(sl_log[2], 3);
}

TEST(sleeps_overlap_rather_than_add) {
    // Three sleeps of 60/20/0ms concurrently: the runtime should take ~60ms, nowhere near
    // the 80ms a serialized run would. The bound is generous (3x) against slow CI.
    sl_n = 0;
    int64_t began = neon_time_monotonic();
    neon_fiber_runtime(sl_parent, NULL);
    int64_t took_ms = (neon_time_monotonic() - began) / 1000000;
    EXPECT(took_ms >= 60);
    EXPECT(took_ms < 180);
}

// ---- the file-offload pool ----

#if defined(__linux__)
// neon_io_read_all through the armed offload hook: the READ ITSELF blocks (a pipe with
// nothing in it yet), but it blocks a WORKER THREAD, not the scheduler — the sibling fiber
// runs, writes, and closes while the reader is parked. Deterministic: the reader is queued
// first and cannot complete until the sibling has run.
static int off_pipe[2];
static bool off_sibling_ran;
static bool off_read_ok;

static void off_reader(void* arg) {
    (void)arg;
    int64_t err = 0;
    neon_str s = neon_io_read_all(off_pipe[0], &err); // offloaded; worker blocks on the pipe
    off_read_ok = err == 0 && neon_str_len(&s) == 2 && neon_str_data(&s)[0] == 'h' &&
                  off_sibling_ran;
    neon_str_release(s);
}
static void off_writer(void* arg) {
    (void)arg;
    off_sibling_ran = true;
    ssize_t w = write(off_pipe[1], "hi", 2);
    (void)w;
    close(off_pipe[1]); // EOF ends the read_all loop
}
static void off_parent(void* arg) {
    (void)arg;
    neon_fiber_spawn(off_reader, NULL);
    neon_fiber_spawn(off_writer, NULL);
}

TEST(read_all_offloads_and_parks_the_fiber) {
    if (pipe(off_pipe) != 0) {
        EXPECT(false);
        return;
    }
    off_sibling_ran = false;
    off_read_ok = false;
    neon_fiber_runtime(off_parent, NULL);
    EXPECT(off_read_ok);
    close(off_pipe[0]);
}
#endif
