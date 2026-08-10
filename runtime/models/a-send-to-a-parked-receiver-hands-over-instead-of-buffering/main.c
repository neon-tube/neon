// Model: a receive on an empty channel, with the send arriving WHILE it is parked.
//
// THE INVARIANT: a value is delivered exactly once. When a receiver is parked, the send
// hands the value straight to it and the ring is not touched; the receiver learns it got a
// value rather than a close; and the waiter is off the list, so the NEXT send buffers
// instead of writing into a stack frame that has gone.
//
// The double-delivery this rules out is not a lost value, it is a use-after-return. A
// waiter lives on the parked receiver's own stack (`neon_chan_recv`'s `w` is a local), which
// is valid exactly while that fiber is parked. If `neon_chan_send` delivered to `ch->rhead`
// without unlinking it, the second send would write `*w->slot` through a frame whose fiber
// has resumed and returned — a wild store into whatever that stack now holds. The
// symmetrical bug, buffering a value that a parked receiver was also handed, leaves the
// receiver holding it and the ring holding it too.
//
// ---- how a single-threaded model parks ----
//
// `neon_fiber_park` is the seam, and the harness's stub is the model's whole scheduler: it
// runs the OTHER fiber. That is not a simplification of the M=1 scheduler, it is what the
// M=1 scheduler does — `neon_fiber_park` marks the fiber blocked and yields to the pump,
// which resumes somebody else, and that somebody else is what sends. The stub runs one
// scripted step per park (`g_step`), so the sequence the receiver is subjected to is chosen
// here rather than nondeterministic: a park is not a place a model wants a solver to explore
// (README, "Performance"), and the two arms worth reaching — a send arrives, a close arrives
// — are two runs of a two-line script, not a search.
//
// What this consequently does NOT model is interleaving: see SCOPE 1.
//
// Verifies `src/fiber_chan.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Baseline: 428 properties, VERIFICATION SUCCESSFUL, 0.3s. Mutations applied to a COPY of
// `src/fiber_chan.c` (the shipping tree is never edited), each run against this harness:
//
// 1. `neon_chan_send`'s rendezvous arm disabled (`if (ch->rhead != NULL)` -> `if (false)`),
//    so every send buffers. Failed 26 of 428: the receiver is never woken, its park returns
//    with `delivered` false, and — the loud part — the waiter is still linked when the
//    channel is later closed, so `neon_chan_close` walks a stack frame that has returned and
//    `--pointer-check` reports "dead object in w->next".
// 2. THE ONE THIS MODEL IS FOR. `ch->rhead = w->next` and the `rtail` fixup dropped, so the
//    send delivers without unlinking. Failed 34 of 410, and the first-class failure is
//    exactly the use-after-return: "dereference failure: dead object in ch->rtail->next", in
//    `neon_chan_recv` — the SECOND receive tries to link itself behind a waiter whose frame
//    is gone. A wild store, and nothing in a test would show it without ASan and luck.
// 3. `w->delivered = true` dropped from the handoff. Failed 1 of 421 on "the parked receiver
//    learns it was handed a value, not closed": the value lands in the receiver's slot and
//    the receiver reports the channel closed and drained. The value is silently lost.
// 4. `ch->rhead = NULL; ch->rtail = NULL;` dropped from the end of `neon_chan_close`, so a
//    close wakes its receivers but keeps them listed. Failed 20 of 417: the second (legal,
//    idempotent) close walks the dead frames, plus "a second close has nobody left to wake".
//    This mutation passed SUCCESSFUL until that second close was added to the harness — the
//    stale list is invisible unless something looks at it again.
//
// NOT caught, and correctly so (scope, not blindness): `w->delivered = false` dropped from
// `neon_chan_close`. The field is initialised false by the receiver and only a delivery ever
// sets it, so restating it is belt-and-braces; SUCCESSFUL, 421 of 421. Reaching it would need
// a close racing a delivery for the SAME waiter, which the M=1 discipline makes unreachable
// (a send that delivers unlinks first, under no interleaving).
//
// ---- SCOPE: what this model does not cover ----
//
// 1. NO INTERLEAVING. The park stub runs the other fiber to completion at a point this
//    harness chooses, which is exactly the M=1 cooperative scheduler's behaviour — one fiber
//    runs at a time and a park is the only switch point (see the file header of
//    `src/fiber_chan.c`). It is NOT a model of the M:N build, where a per-channel lock
//    arrives and a send can run concurrently with a receive: nothing here says the lock is in
//    the right places.
// 2. ONE PARKED RECEIVER. The waiter list therefore never has two entries, so the FIFO order
//    of receivers — a second receiver must not be handed a value ahead of a first — is not
//    verified. Reaching it means a park inside a park, i.e. the harness scripting a fiber
//    tree, which is a different model and a much deeper one.
// 3. THE SCHEDULER SIDE IS THE HARNESS'S. `neon_fiber_park` returning is this model's
//    definition of "and then it was woken"; whether `neon_fiber_wake` actually re-admits the
//    fiber is a separate property, and it has its own model —
//    `a-second-wake-does-not-admit-an-already-queued-fiber-twice`.
// 4. THE LANGUAGE CHANNEL'S RENDEZVOUS IS NOT VERIFIED HERE. `neon_channel_send_impl`'s
//    handoff additionally re-points `neon_current_arena` at the parked receiver's arena and
//    deep-copies through the element witness. That path is out of CBMC's reach — see SCOPE 2
//    of `chan-ring-preserves-fifo-order-across-a-wrapping-growth` for the mechanism and the
//    measurements.

#include "../support/cbmc_support.h"
#include "libneon_rt.h"

#include "neon/channel.h"

#include <stdio.h>
#include <stdlib.h>

int fprintf(FILE* stream, const char* fmt, ...) { (void)stream; (void)fmt; return 0; }
int fflush(FILE* stream) { (void)stream; return 0; }

#define NVAL 4
static int g_val[NVAL];

static neon_chan* g_ch;
static int g_step;
static int g_parks;
static int g_wakes;

neon_fiber* neon_fiber_current(void) { return NULL; }
neon_fiber* neon_fiber_spawn(neon_fiber_fn fn, void* arg) { (void)fn; (void)arg; return NULL; }

// A wake here only records that it happened: the fiber it names is the one whose park is
// about to return, and the harness resumes it by returning from the stub below.
void neon_fiber_wake(neon_fiber* f) { (void)f; g_wakes++; }

// The other fiber's turn. One scripted step per park.
void neon_fiber_park(void) {
    g_parks++;
    switch (g_step) {
    case 1:
        neon_chan_send(g_ch, &g_val[1]); // a value arrives for the parked receiver
        break;
    case 2:
        neon_chan_close(g_ch); // ...or a close does, and it goes home empty-handed
        break;
    default:
        PROVE(0, "the receiver only parks where this model expects it to");
        break;
    }
}

int main(void) {
    g_ch = neon_chan_new();

    // ---- the handoff ----
    g_step = 1;
    void* out = &g_val[0];
    bool got = neon_chan_recv(g_ch, &out);

    PROVE(g_parks == 1, "an empty, open channel parks the receiver exactly once");
    PROVE(g_wakes == 1, "the send wakes exactly one receiver");
    PROVE(got, "the parked receiver learns it was handed a value, not closed");
    PROVE(out == &g_val[1], "and the value it gets is the one that was sent");

    // ---- the waiter is off the list ----
    //
    // The next send finds no receiver, so it must BUFFER. If it still saw the old waiter it
    // would write through a stack slot that has gone; here that shows up as the value never
    // reaching the ring.
    g_step = 0; // any further park is a model error
    neon_chan_send(g_ch, &g_val[2]);

    PROVE(g_parks == 1, "a send with nobody parked does not park anyone");
    PROVE(g_wakes == 1, "and wakes nobody");

    void* out2 = &g_val[0];
    PROVE(neon_chan_recv(g_ch, &out2), "the value sent after the handoff was buffered");
    PROVE(out2 == &g_val[2], "and comes back out of the ring intact");

    // ---- the ring holds nothing else ----
    //
    // In particular the handed-over value was never ALSO buffered: the channel is empty, and
    // a closed empty channel is the only state in which recv reports false without parking.
    g_step = 0;
    neon_chan_close(g_ch);
    void* out3 = &g_val[3];
    PROVE(!neon_chan_recv(g_ch, &out3), "nothing else was ever buffered");
    PROVE(out3 == &g_val[3], "a receive that returns false leaves the out-parameter alone");
    PROVE(g_parks == 1, "a closed channel never parks a receiver");

    neon_chan_free(g_ch);

    // ---- the other arm: the close wins the race ----
    g_ch = neon_chan_new();
    g_step = 2;
    g_parks = 0;
    g_wakes = 0;
    void* out4 = &g_val[3];
    bool got4 = neon_chan_recv(g_ch, &out4);

    PROVE(g_parks == 1, "the receiver parks on the open, empty channel");
    PROVE(g_wakes == 1, "close wakes the parked receiver");
    PROVE(!got4, "a receiver woken by close reports no value");
    PROVE(out4 == &g_val[3], "and close never writes into the receiver's slot");

    // Close is documented idempotent, and this second one is the oracle for "close emptied
    // the receiver list": the frame holding that waiter has RETURNED, so a close that still
    // had it linked would walk a dead object. `--pointer-check` says so in those words.
    neon_chan_close(g_ch);
    PROVE(g_wakes == 1, "a second close has nobody left to wake");

    neon_chan_free(g_ch);
    return 0;
}
