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
