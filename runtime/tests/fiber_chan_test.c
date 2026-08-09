// Channels and Task/await (slice 5 of docs/design/fibers.md): fibers blocking on and waking
// each other through the scheduler's park/wake. Payloads are scalars cast to void* — no arena
// lifetime to worry about — so these test the mechanism (buffering, rendezvous, close, task
// completion) rather than value-copy semantics, which are codegen's job.

#include "tinyunit.h"

#include "support.h"

#include "neon/channel.h"
#include "neon/fiber.h"

#include <stdint.h>

TEST_SUITE("fiber_chan");

// Buffered send then receive, all in one fiber: FIFO order, and a closed+empty receive is
// false.
static int buf_sum;
static void buffered_body(void* arg) {
    (void)arg;
    neon_chan* ch = neon_chan_new();
    neon_chan_send(ch, (void*)(intptr_t)10);
    neon_chan_send(ch, (void*)(intptr_t)20);
    void* v;
    neon_chan_recv(ch, &v);
    buf_sum += (int)(intptr_t)v; // 10 first
    neon_chan_recv(ch, &v);
    buf_sum += (int)(intptr_t)v; // then 20
    neon_chan_close(ch);
    bool more = neon_chan_recv(ch, &v); // closed and empty
    buf_sum += more ? 1000 : 0;
    neon_chan_free(ch);
}

TEST(buffered_send_recv_is_fifo) {
    buf_sum = 0;
    neon_fiber_runtime(buffered_body, NULL);
    EXPECT_EQ(buf_sum, 30); // 10 + 20, and the closed receive added nothing
}

// A receive on an empty channel parks until a send in another fiber wakes it.
static neon_chan* rv_ch;
static int rv_got;
static void rv_receiver(void* arg) {
    (void)arg;
    void* v;
    if (neon_chan_recv(rv_ch, &v)) { // blocks: the channel is empty when this runs
        rv_got = (int)(intptr_t)v;
    }
}
static void rv_sender(void* arg) {
    (void)arg;
    neon_chan_send(rv_ch, (void*)(intptr_t)42); // hands the parked receiver its value
}
static void rv_parent(void* arg) {
    (void)arg;
    rv_ch = neon_chan_new();
    neon_fiber_spawn(rv_receiver, NULL); // runs first, blocks
    neon_fiber_spawn(rv_sender, NULL);   // runs next, wakes it
}

TEST(recv_blocks_until_a_send) {
    rv_got = 0;
    neon_fiber_runtime(rv_parent, NULL);
    EXPECT_EQ(rv_got, 42);
    neon_chan_free(rv_ch);
}

// Closing wakes a parked receiver with a false return, not a value.
static neon_chan* cl_ch;
static int cl_result;
static void cl_receiver(void* arg) {
    (void)arg;
    void* v;
    cl_result = neon_chan_recv(cl_ch, &v) ? 1 : 0;
}
static void cl_closer(void* arg) {
    (void)arg;
    neon_chan_close(cl_ch);
}
static void cl_parent(void* arg) {
    (void)arg;
    cl_ch = neon_chan_new();
    neon_fiber_spawn(cl_receiver, NULL); // blocks on the empty channel
    neon_fiber_spawn(cl_closer, NULL);   // closes it, waking the receiver
}

TEST(close_wakes_a_parked_receiver_with_false) {
    cl_result = -1;
    neon_fiber_runtime(cl_parent, NULL);
    EXPECT_EQ(cl_result, 0);
    neon_chan_free(cl_ch);
}

// One producer, one consumer, five values then close: the consumer sums them and stops at the
// closed-and-drained false.
static neon_chan* pc_ch;
static int pc_sum;
static void pc_producer(void* arg) {
    (void)arg;
    for (int i = 1; i <= 5; i++) {
        neon_chan_send(pc_ch, (void*)(intptr_t)i);
    }
    neon_chan_close(pc_ch);
}
static void pc_consumer(void* arg) {
    (void)arg;
    void* v;
    while (neon_chan_recv(pc_ch, &v)) {
        pc_sum += (int)(intptr_t)v;
    }
}
static void pc_parent(void* arg) {
    (void)arg;
    pc_ch = neon_chan_new();
    neon_fiber_spawn(pc_consumer, NULL);
    neon_fiber_spawn(pc_producer, NULL);
}

TEST(producer_consumer_over_a_channel) {
    pc_sum = 0;
    neon_fiber_runtime(pc_parent, NULL);
    EXPECT_EQ(pc_sum, 1 + 2 + 3 + 4 + 5);
    neon_chan_free(pc_ch);
}

// ---- Task / await ----

static void* square(void* arg) {
    intptr_t n = (intptr_t)arg;
    return (void*)(intptr_t)(n * n);
}

static int task_result;
static void await_parent(void* arg) {
    (void)arg;
    neon_task* t = neon_task_spawn(square, (void*)(intptr_t)7);
    task_result = (int)(intptr_t)neon_task_await(t); // blocks until the task finishes
    neon_task_free(t);
}

TEST(task_await_returns_the_result) {
    task_result = 0;
    neon_fiber_runtime(await_parent, NULL);
    EXPECT_EQ(task_result, 49);
}

static void await_after_done_parent(void* arg) {
    (void)arg;
    neon_task* t = neon_task_spawn(square, (void*)(intptr_t)5);
    neon_fiber_sched_yield(); // let the task run to completion first
    task_result = (int)(intptr_t)neon_task_await(t); // already done: returns at once
    neon_task_free(t);
}

TEST(await_on_a_finished_task_is_immediate) {
    task_result = 0;
    neon_fiber_runtime(await_after_done_parent, NULL);
    EXPECT_EQ(task_result, 25);
}

static void two_tasks_parent(void* arg) {
    (void)arg;
    neon_task* a = neon_task_spawn(square, (void*)(intptr_t)3);
    neon_task* b = neon_task_spawn(square, (void*)(intptr_t)4);
    int ra = (int)(intptr_t)neon_task_await(a);
    int rb = (int)(intptr_t)neon_task_await(b);
    task_result = ra + rb;
    neon_task_free(a);
    neon_task_free(b);
}

TEST(awaiting_two_tasks) {
    task_result = 0;
    neon_fiber_runtime(two_tasks_parent, NULL);
    EXPECT_EQ(task_result, 9 + 16);
}
