// select_recv: receive from whichever of several language channels is ready first
// (src/fiber_chan.c). Like fiber_chan_test.c, payloads are i64 so the tests exercise the
// mechanism — the ready-value snapshot, the parked rendezvous, the claim that makes exactly
// one channel win, and the close path — not value-copy semantics, which are codegen's job.
//
// These run under BOTH IO engines (ctest, then NEON_IO=epoll) and under LeakSanitizer, which
// is the oracle for the refcount discipline: every minted per-holder ref is consumed by the
// list the select is handed (or by the op it is passed to), so a leak here is a real bug.

#include "tinyunit.h"

#include "support.h"

#include "neon/channel.h"
#include "neon/fiber.h"
#include "neon/list.h"

#include <stdint.h>

TEST_SUITE("fiber_select");

// The element witness for a `List[Channel[T]]`: each slot holds a per-holder channel ref, so
// retain/release forward to the ref's own count and `copy` is the same fresh-ref mint the
// compiler emits when a channel crosses a boundary. Size is a pointer's.
static void nt_chanref_retain(void* p) { neon_retain(*(neon_header**)p); }
static void nt_chanref_release(void* p) { neon_release(*(neon_header**)p); }
static const neon_witness nt_chanref_w = {sizeof(void*),      nt_chanref_retain,
                                          nt_chanref_release, NULL,
                                          NULL,               neon_wcopy_channel};

// Build a fresh `List[Channel[i64]]` over `n` channels, minting one ref per entry (moved into
// the list, which then owns it). The select op consumes the returned list.
static neon_list* select_list(neon_channel_ref* const* chans, int n) {
    neon_list* l = neon_list_new(&nt_chanref_w);
    for (int i = 0; i < n; i++) {
        neon_channel_ref* r = nt_chan_handle(&chans[i]);
        l = neon_list_push(l, &r);
    }
    return l;
}

// ---- a value already buffered: select takes it without parking ----

static neon_channel_ref* rd_ch[3];
static int64_t rd_idx;
static int64_t rd_val;
static bool rd_got;
static void rd_body(void* arg) {
    (void)arg;
    int64_t v = 77;
    neon_channel_send(nt_chan_handle(&rd_ch[2]), &v); // buffered on channel index 2, no receiver
    int64_t out = -1;
    rd_got = neon_channel_select_recv(select_list(rd_ch, 3), &out, &rd_idx);
    rd_val = out;
}

TEST(select_returns_the_ready_channels_index_and_value) {
    for (int i = 0; i < 3; i++) {
        rd_ch[i] = neon_channel_new(&nt_i64_w);
    }
    rd_idx = -1;
    rd_val = 0;
    rd_got = false;
    neon_fiber_runtime(rd_body, NULL);
    EXPECT(rd_got);        // a value, not a close
    EXPECT_EQ(rd_idx, 2);  // the channel that held it
    EXPECT_EQ(rd_val, 77);
    for (int i = 0; i < 3; i++) {
        neon_release((neon_header*)rd_ch[i]);
    }
}

// ---- values on two channels: the lowest index wins (deterministic) ----

static neon_channel_ref* lo_ch[3];
static int64_t lo_idx;
static int64_t lo_val;
static void lo_body(void* arg) {
    (void)arg;
    int64_t a = 10, b = 20;
    neon_channel_send(nt_chan_handle(&lo_ch[2]), &a); // buffered on 2
    neon_channel_send(nt_chan_handle(&lo_ch[1]), &b); // buffered on 1
    int64_t out = -1;
    neon_channel_select_recv(select_list(lo_ch, 3), &out, &lo_idx);
    lo_val = out;
}

TEST(select_prefers_the_lowest_index_ready_value) {
    for (int i = 0; i < 3; i++) {
        lo_ch[i] = neon_channel_new(&nt_i64_w);
    }
    lo_idx = -1;
    lo_val = 0;
    neon_fiber_runtime(lo_body, NULL);
    EXPECT_EQ(lo_idx, 1);  // index 1 beats index 2
    EXPECT_EQ(lo_val, 20);
    for (int i = 0; i < 3; i++) {
        neon_release((neon_header*)lo_ch[i]);
    }
}

// ---- parked on several channels, woken by a send to one of them ----

static neon_channel_ref* wk_ch[3];
static int64_t wk_idx;
static int64_t wk_val;
static bool wk_got;
static void wk_consumer(void* arg) {
    (void)arg;
    int64_t out = -1;
    wk_got = neon_channel_select_recv(select_list(wk_ch, 3), &out, &wk_idx); // parks: all empty
    wk_val = out;
}
static void wk_sender(void* arg) {
    (void)arg;
    int64_t v = 55;
    neon_channel_send(nt_chan_handle(&wk_ch[1]), &v); // rendezvous with the parked select
}
static void wk_parent(void* arg) {
    (void)arg;
    neon_fiber_spawn(wk_consumer, NULL); // runs first, parks on all three
    neon_fiber_spawn(wk_sender, NULL);   // delivers to channel 1, waking it
}

TEST(select_wakes_when_a_send_reaches_one_parked_channel) {
    for (int i = 0; i < 3; i++) {
        wk_ch[i] = neon_channel_new(&nt_i64_w);
    }
    wk_idx = -1;
    wk_val = 0;
    wk_got = false;
    neon_fiber_runtime(wk_parent, NULL);
    EXPECT(wk_got);
    EXPECT_EQ(wk_idx, 1);
    EXPECT_EQ(wk_val, 55);
    for (int i = 0; i < 3; i++) {
        neon_release((neon_header*)wk_ch[i]);
    }
}

// ---- a closed channel in the set returns its index with the null outcome ----

static neon_channel_ref* cl_ch[2];
static int64_t cl_idx;
static bool cl_got;
static void cl_consumer(void* arg) {
    (void)arg;
    int64_t out = -1;
    cl_got = neon_channel_select_recv(select_list(cl_ch, 2), &out, &cl_idx);
}
static void cl_parent(void* arg) {
    (void)arg;
    neon_channel_close(nt_chan_handle(&cl_ch[1])); // channel 1 closed and empty
    neon_fiber_spawn(cl_consumer, NULL);           // select sees it drained-closed at once
}

TEST(select_returns_a_closed_channel_with_null) {
    cl_ch[0] = neon_channel_new(&nt_i64_w);
    cl_ch[1] = neon_channel_new(&nt_i64_w);
    cl_idx = -1;
    cl_got = true;
    neon_fiber_runtime(cl_parent, NULL);
    EXPECT(!cl_got);      // the null outcome (closed and drained)
    EXPECT_EQ(cl_idx, 1); // that channel's index
    neon_release((neon_header*)cl_ch[0]);
    neon_release((neon_header*)cl_ch[1]);
}

// A ready value BEATS a closed channel even at a lower index: channel 0 closed, channel 1 has
// a value → select delivers the value, index 1, not the close at 0.
static neon_channel_ref* vb_ch[2];
static int64_t vb_idx;
static int64_t vb_val;
static bool vb_got;
static void vb_consumer(void* arg) {
    (void)arg;
    int64_t out = -1;
    vb_got = neon_channel_select_recv(select_list(vb_ch, 2), &out, &vb_idx);
    vb_val = out;
}
static void vb_parent(void* arg) {
    (void)arg;
    int64_t v = 88;
    neon_channel_send(nt_chan_handle(&vb_ch[1]), &v); // a value on channel 1
    neon_channel_close(nt_chan_handle(&vb_ch[0]));    // channel 0 closed and empty
    neon_fiber_spawn(vb_consumer, NULL);
}

TEST(select_prefers_a_value_over_a_closed_channel) {
    vb_ch[0] = neon_channel_new(&nt_i64_w);
    vb_ch[1] = neon_channel_new(&nt_i64_w);
    vb_idx = -1;
    vb_val = 0;
    vb_got = false;
    neon_fiber_runtime(vb_parent, NULL);
    EXPECT(vb_got);        // a value, not the close at index 0
    EXPECT_EQ(vb_idx, 1);
    EXPECT_EQ(vb_val, 88);
    neon_release((neon_header*)vb_ch[0]);
    neon_release((neon_header*)vb_ch[1]);
}

// ---- no double delivery: two channels racing to hand one select a value ----
//
// The consumer parks on channels 0 and 1. Sender A delivers to channel 0 (claims the select,
// waking it), then — before the woken select runs — sender B sends to channel 1. B must find
// the select already claimed, skip its stale waiter, and BUFFER its value: the select takes
// exactly channel 0's value, and channel 1 still holds B's, which a following recv confirms.
// Under one seat the run order (consumer, A, B, then the re-woken select) is deterministic.
static neon_channel_ref* dd_ch[2];
static int64_t dd_idx;
static int64_t dd_sel_val;
static bool dd_sel_got;
static int64_t dd_ch1_val;
static bool dd_ch1_got;
static void dd_consumer(void* arg) {
    (void)arg;
    int64_t out = -1;
    dd_sel_got = neon_channel_select_recv(select_list(dd_ch, 2), &out, &dd_idx); // parks
    dd_sel_val = out;
    int64_t out2 = -1;
    dd_ch1_got = neon_channel_recv(nt_chan_handle(&dd_ch[1]), &out2); // B's value, still buffered
    dd_ch1_val = out2;
}
static void dd_sender_a(void* arg) {
    (void)arg;
    int64_t v = 11;
    neon_channel_send(nt_chan_handle(&dd_ch[0]), &v); // rendezvous: wins the select
}
static void dd_sender_b(void* arg) {
    (void)arg;
    int64_t v = 99;
    neon_channel_send(nt_chan_handle(&dd_ch[1]), &v); // select already claimed: buffers instead
}
static void dd_parent(void* arg) {
    (void)arg;
    neon_fiber_spawn(dd_consumer, NULL);
    neon_fiber_spawn(dd_sender_a, NULL);
    neon_fiber_spawn(dd_sender_b, NULL);
}

TEST(select_delivers_from_exactly_one_channel) {
    dd_ch[0] = neon_channel_new(&nt_i64_w);
    dd_ch[1] = neon_channel_new(&nt_i64_w);
    dd_idx = -1;
    dd_sel_val = 0;
    dd_sel_got = false;
    dd_ch1_val = 0;
    dd_ch1_got = false;
    neon_fiber_runtime(dd_parent, NULL);
    EXPECT(dd_sel_got);        // the select got a value
    EXPECT_EQ(dd_idx, 0);      // from channel 0 (sender A), the one that claimed it
    EXPECT_EQ(dd_sel_val, 11);
    EXPECT(dd_ch1_got);        // sender B's value was NOT delivered to the select —
    EXPECT_EQ(dd_ch1_val, 99); // it stayed buffered on channel 1, drained here
    neon_release((neon_header*)dd_ch[0]);
    neon_release((neon_header*)dd_ch[1]);
}
