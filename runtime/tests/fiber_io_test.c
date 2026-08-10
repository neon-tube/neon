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

// ---- the mesh (M:N) ----

#if defined(__linux__)
#include <stdatomic.h>

// Two seats, four fibers: every fiber runs (the atomic count proves the remote seats ran
// theirs), and a channel pipeline crosses the mesh — the consumer and producer land on
// DIFFERENT seats by round-robin, so every send is a cross-thread handoff.
static _Atomic int mn_ran;
static void mn_worker(void* arg) {
    (void)arg;
    atomic_fetch_add(&mn_ran, 1);
}
static void mn_parent(void* arg) {
    (void)arg;
    for (int i = 0; i < 4; i++) {
        neon_fiber_spawn(mn_worker, NULL);
    }
}

TEST(runtime_threads_runs_fibers_on_all_seats) {
    atomic_store(&mn_ran, 0);
    neon_fiber_runtime_threads(2, mn_parent, NULL);
    EXPECT_EQ(atomic_load(&mn_ran), 4);
}

static neon_channel* mnc_ch;
static const neon_witness mnc_i64_w = {sizeof(int64_t), NULL, NULL, nt_i64_eq, nt_i64_cmp};
static long mnc_sum;
static void mnc_consumer(void* arg) {
    (void)arg;
    int64_t v;
    neon_retain_shared((neon_header*)mnc_ch);
    while (neon_channel_recv(mnc_ch, &v)) {
        mnc_sum += v;
        neon_retain_shared((neon_header*)mnc_ch);
    }
}
static void mnc_producer(void* arg) {
    (void)arg;
    for (int64_t i = 1; i <= 100; i++) {
        neon_retain_shared((neon_header*)mnc_ch);
        neon_channel_send(mnc_ch, &i);
    }
    neon_retain_shared((neon_header*)mnc_ch);
    neon_channel_close(mnc_ch);
}
static void mnc_parent(void* arg) {
    (void)arg;
    neon_fiber_spawn(mnc_consumer, NULL); // seat A
    neon_fiber_spawn(mnc_producer, NULL); // seat B: every handoff crosses threads
}

TEST(a_channel_crosses_the_mesh) {
    mnc_ch = neon_channel_new(&mnc_i64_w);
    mnc_sum = 0;
    neon_fiber_runtime_threads(2, mnc_parent, NULL);
    EXPECT_EQ(mnc_sum, 5050L);
    neon_release_shared((neon_header*)mnc_ch);
}

// Sleeps on both seats overlap across the mesh exactly as they do on one.
static _Atomic int mns_done;
static void mns_sleeper(void* arg) {
    (void)arg;
    neon_fiber_sleep(30);
    atomic_fetch_add(&mns_done, 1);
}
static void mns_parent(void* arg) {
    (void)arg;
    neon_fiber_spawn(mns_sleeper, NULL);
    neon_fiber_spawn(mns_sleeper, NULL);
    neon_fiber_spawn(mns_sleeper, NULL);
}

TEST(sleeps_overlap_across_the_mesh) {
    atomic_store(&mns_done, 0);
    int64_t began = neon_time_monotonic();
    neon_fiber_runtime_threads(2, mns_parent, NULL);
    int64_t took = (neon_time_monotonic() - began) / 1000000;
    EXPECT_EQ(atomic_load(&mns_done), 3);
    EXPECT(took < 90); // three 30ms sleeps overlapped, not summed
}
#endif

// ---- per-thread preemption (M:N) ----

#if defined(__linux__)
// TWO spinners, one per seat, neither yielding voluntarily; each must be preempted by ITS
// OWN thread's timer so its sibling on the same seat runs. Under the old process-wide timer
// only one thread caught the tick and the other seat's spinner starved its sibling forever.
static _Atomic int pt_siblings;
static void pt_spinner(void* arg) {
    (void)arg;
    long id = (long)arg;
    for (long i = 0; i < 2000000000L; i++) {
        // stop once BOTH siblings have run — proves both seats preempted
        if (atomic_load(&pt_siblings) >= 2) {
            break;
        }
        neon_fiber_safepoint();
    }
    (void)id;
}
static void pt_sibling(void* arg) {
    (void)arg;
    atomic_fetch_add(&pt_siblings, 1);
}
static void pt_parent(void* arg) {
    (void)arg;
    // Four fibers over two seats: round-robin puts a spinner and a sibling on each seat.
    neon_fiber_spawn(pt_spinner, (void*)1);
    neon_fiber_spawn(pt_spinner, (void*)2);
    neon_fiber_spawn(pt_sibling, NULL);
    neon_fiber_spawn(pt_sibling, NULL);
}

TEST(both_seats_preempt_their_spinners) {
    atomic_store(&pt_siblings, 0);
    neon_fiber_runtime_threads(2, pt_parent, NULL);
    EXPECT_EQ(atomic_load(&pt_siblings), 2); // both siblings ran → both seats' timers fired
}
#endif

// ---- the io_uring backend ----
//
// These run against WHICHEVER engine the seat opened — the ring on a modern kernel, the
// epoll+offload path under NEON_IO=epoll or an old one. That is the point: the backend is
// a seam, and the behaviour above it is identical, so the same assertions hold either way.
// The CI script runs the suite twice, once with NEON_IO=epoll.

#if defined(__linux__)
// A read that must block: the pipe is empty when the reader runs, so the operation is
// genuinely in flight (a ring SQE, or a pool worker) while a sibling fiber writes.
static int ur_pipe[2];
static bool ur_sibling_ran;
static bool ur_ok;

static void ur_reader(void* arg) {
    (void)arg;
    int64_t err = 0;
    neon_str s = neon_io_read_all(ur_pipe[0], &err);
    ur_ok = err == 0 && neon_str_len(&s) == 5 &&
            memcmp(neon_str_data(&s), "uring", 5) == 0 && ur_sibling_ran;
    neon_str_release(s);
}
static void ur_writer(void* arg) {
    (void)arg;
    ur_sibling_ran = true;
    ssize_t w = write(ur_pipe[1], "uring", 5);
    (void)w;
    close(ur_pipe[1]); // EOF ends read_all's loop
}
static void ur_parent(void* arg) {
    (void)arg;
    neon_fiber_spawn(ur_reader, NULL); // parks with the read in flight
    neon_fiber_spawn(ur_writer, NULL); // satisfies it
}

TEST(a_read_completes_through_the_active_engine) {
    if (pipe(ur_pipe) != 0) {
        EXPECT(false);
        return;
    }
    ur_sibling_ran = false;
    ur_ok = false;
    neon_fiber_runtime(ur_parent, NULL);
    EXPECT(ur_ok);
    close(ur_pipe[0]);
}

// Many concurrent reads, so the submission path is exercised in batches rather than one at
// a time — and every fiber must get ITS OWN result back (user_data → the right waiter).
enum { UR_N = 24 };
static int ur_pipes[UR_N][2];
static int ur_got[UR_N];
static void ur_many_reader(void* arg) {
    long i = (long)arg;
    char buf[4];
    int64_t err = 0;
    neon_str s = neon_io_read_all(ur_pipes[i][0], &err);
    ur_got[i] = err == 0 && neon_str_len(&s) == 1 ? (unsigned char)neon_str_data(&s)[0] : -1;
    neon_str_release(s);
    (void)buf;
}
static void ur_many_writer(void* arg) {
    (void)arg;
    for (long i = 0; i < UR_N; i++) {
        char c = (char)(i + 1);
        ssize_t w = write(ur_pipes[i][1], &c, 1);
        (void)w;
        close(ur_pipes[i][1]);
    }
}
static void ur_many_parent(void* arg) {
    (void)arg;
    for (long i = 0; i < UR_N; i++) {
        neon_fiber_spawn(ur_many_reader, (void*)i);
    }
    neon_fiber_spawn(ur_many_writer, NULL);
}

TEST(concurrent_reads_each_get_their_own_result) {
    for (int i = 0; i < UR_N; i++) {
        if (pipe(ur_pipes[i]) != 0) {
            EXPECT(false);
            return;
        }
        ur_got[i] = 0;
    }
    neon_fiber_runtime(ur_many_parent, NULL);
    bool all = true;
    for (int i = 0; i < UR_N; i++) {
        if (ur_got[i] != i + 1) { // fiber i must receive byte i+1, not another's
            all = false;
        }
        close(ur_pipes[i][0]);
    }
    EXPECT(all);
}

// A blocking read and a sleep outstanding at once: the idle wait must serve BOTH — under
// the ring that is one enter() carrying a TIMEOUT SQE alongside the read.
static int urm_pipe[2];
static bool urm_slept;
static bool urm_read_ok;
static void urm_sleeper(void* arg) {
    (void)arg;
    neon_fiber_sleep(25);
    urm_slept = true;
    ssize_t w = write(urm_pipe[1], "x", 1); // the sleeper satisfies the reader
    (void)w;
    close(urm_pipe[1]);
}
static void urm_reader(void* arg) {
    (void)arg;
    int64_t err = 0;
    neon_str s = neon_io_read_all(urm_pipe[0], &err);
    urm_read_ok = err == 0 && neon_str_len(&s) == 1 && urm_slept;
    neon_str_release(s);
}
static void urm_parent(void* arg) {
    (void)arg;
    neon_fiber_spawn(urm_reader, NULL);
    neon_fiber_spawn(urm_sleeper, NULL);
}

TEST(a_deadline_and_an_operation_wait_together) {
    if (pipe(urm_pipe) != 0) {
        EXPECT(false);
        return;
    }
    urm_slept = false;
    urm_read_ok = false;
    neon_fiber_runtime(urm_parent, NULL);
    EXPECT(urm_read_ok); // the read completed, and only after the deadline fired
    close(urm_pipe[0]);
}
#endif
