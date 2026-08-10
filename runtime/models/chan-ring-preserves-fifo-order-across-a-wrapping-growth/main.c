// Model: the channel ring buffer, driven so that its doubling growth happens while the ring
// is WRAPPED — the live run split across the end of the old buffer — and then drained.
//
// THE INVARIANT: values come out of a channel in the order they went in, and the growth step
// preserves every buffered value AND its position in that order, including across the wrap.
//
// Why this is the part worth a machine check. `neon_chan_buffer` is a growable circular
// buffer, and its three index expressions must agree about where the live run is:
//
//     write:  ch->buf[(ch->head + ch->len) % ch->cap] = v
//     read:   *out = ch->buf[ch->head];  ch->head = (ch->head + 1) % ch->cap
//     grow:   nbuf[i] = ch->buf[(ch->head + i) % ch->cap]  ... then head = 0
//
// The growth step is where they can disagree. It LINEARISES: the run that was `head..cap-1`
// followed by `0..` becomes `0..len-1`, and `head` is reset to 0 to match. Get any part of
// that wrong — copy `buf[i]` instead of `buf[(head+i) % cap]`, forget the `head = 0`, size
// the new buffer from `cap` rather than `ncap` — and the channel does not crash: it hands
// back the right VALUES in the wrong ORDER, or hands back a slot that was never written.
// A channel that reorders is not a channel, and nothing about it is loud.
//
// A test can only reach this arm by luck of timing. The harness reaches it on purpose, and
// wraps BOTH indices on the way, because they wrap in different places and one harness can
// easily miss one: draining a full cap-4 ring takes the READ index round (3 -> 0), and a
// refill from `head == 1` takes the WRITE index round — so the growth then finds `len == cap`
// with `head == 1` and a live run of 6,7,8 | 9 rather than 6,7,8,9.
//
// The read wrap was in fact missed by the first version of this harness, which drained only
// two of the four and so never took `head` past 2. Mutation 3 below passed against it. That
// is the model's own scope note earning its keep: the sequence is chosen to reach a state,
// not to look plausible.
//
// Two further properties ride along for free and are worth naming, because they are the ones
// that would be memory-unsafety rather than a wrong answer:
//
//   * `len <= cap` — a write past the ring is a heap overflow, and `--pointer-check` over the
//     malloc'd `buf` is the oracle for it. Mutation 4 below confirms it fires.
//   * no slot is read before it is written — an unwritten slot yields an unconstrained value,
//     which no order assertion can accidentally satisfy.
//
// Values are addresses of distinct static ints, so "the right value" is pointer identity: a
// duplicated read, a skipped slot and a torn one are all distinguishable, not merely unequal.
//
// Verifies `src/fiber_chan.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Baseline: 424 properties, VERIFICATION SUCCESSFUL, 0.3s. Mutations applied to a COPY of
// `src/fiber_chan.c` (the shipping tree is never edited), each run against this harness:
//
// 1. `neon_chan_buffer`'s growth copy delinearised: `nbuf[i] = ch->buf[(ch->head + i) %
//    ch->cap]` -> `nbuf[i] = ch->buf[i]`. This is the whole point of the model. Failed 4 of
//    408, first on "growth keeps the oldest value oldest" — the channel comes back with
//    9,6,7,8 where 6,7,8,9 went in. Under a non-wrapped ring (head == 0) this mutation is
//    INVISIBLE, which is exactly why the harness wraps.
// 2. `ch->head = 0` dropped after the growth. Failed 5 of 418: the copy linearised the run
//    but the read index still points at 1, so the drain starts one past the front of it and
//    then walks into a slot the growth never wrote ("every value received is one this model
//    sent").
// 3. `neon_chan_recv`'s advance `ch->head = (ch->head + 1) % ch->cap` -> `ch->head + 1`,
//    the missing read wrap. Failed 3 of 417 — a `--pointer-check` dereference failure in
//    `neon_chan_recv` once `head` runs off the end of the allocation, plus the order and
//    provenance assertions. Passed SUCCESSFUL against the first version of this harness; see
//    the note above.
// 4. `malloc(ncap * sizeof(void*))` -> `malloc(ch->cap * sizeof(void*))`, the classic
//    off-by-a-doubling. Failed 17 of 424 — six of them `--pointer-check` failures on the
//    growth copy, on the buffered write after it and on the following read: a heap overflow,
//    caught as memory unsafety and not merely as a wrong answer.
//
// NOT caught, and correctly so (scope, not blindness): `if (ch->len == ch->cap)` weakened to
// `<=`, so every send reallocates. That is a performance bug, not a correctness one — the
// ring stays consistent — and this model verifies SUCCESSFUL with it, 424 of 424.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. SINGLE-THREADED, AND NO PARKING. `neon_chan_recv` parks when the ring is empty and
//    `neon_chan_send` hands a parked receiver the value directly; this harness keeps the ring
//    non-empty at every receive, so neither arm runs, and `neon_fiber_park`/`wake`/`current`
//    are harness stubs. That is deliberate: the ring is a data structure, and a
//    single-threaded model of a data structure is a real proof OF THAT DATA STRUCTURE. It is
//    not a proof that the scheduler's park/wake around it is correct, and this model claims
//    nothing about interleaving, the M:N per-channel lock, or the rendezvous fast path.
// 2. THE LANGUAGE CHANNEL'S RING IS A DIFFERENT COPY OF THIS ALGORITHM AND IS NOT VERIFIED
//    HERE. `neon_channel_slot_reserve` (same file) does the same linearising growth over
//    witness-sized slots. It is out of CBMC's reach for the reason README's "heap-allocated
//    containers" section describes: its body is `neon_alloc`'d, so `ch->w` and the object's
//    own `drop` read back symbolically, and the indirect calls through them (`ch->w->copy`,
//    `ch->w->release`, `h->drop`) branch over every address-taken `void (*)(void*)` in the
//    translation unit — including `neon_task_lang_body`, which declares a VLA of symbolic
//    size (`char tmp[t->w->size]`) and calls another function pointer, and
//    `neon_channel_drop`, which calls `ch->w->release` again and so re-enters the same
//    switch. Measured, CBMC 6.10.0: a harness doing nothing but `neon_channel_new` +
//    `neon_channel_close` does not finish SYMBOLIC EXECUTION in 120s (it never reaches the
//    solver); 60s of it logs 193704 symex entries into `neon_task_lang_body` alone. The
//    cause is upstream of the drop: `neon_channel_close_impl`'s `for (w = ch->rhead; ...)`
//    unwinds to the full bound over a field that was written NULL a moment earlier, so field
//    values are not being propagated out of the object at all. Replacing `neon_alloc` with a
//    harness-typed allocator does not help (still >120s), and the same 20-line experiment
//    with a plain `S* s = malloc(sizeof(S))` DOES propagate — so the boundary is the
//    allocation's declared type, not heap objects as such. Recorded so it is not
//    rediscovered.
// 3. ONE GROWTH STEP. cap goes 0 -> 4 -> 8 and stops; repeated doubling, and any `cap` large
//    enough for `cap * 2` or `ncap * sizeof(void*)` to approach `size_t` overflow, are not
//    reached.
// 4. The values are opaque pointers the channel never dereferences, so there is no ownership
//    accounting here — `neon_chan` does not own what it carries (see `neon/channel.h`).

#include "../support/cbmc_support.h"
#include "libneon_rt.h"

#include "neon/channel.h"

#include <stdio.h>
#include <stdlib.h>

int fprintf(FILE* stream, const char* fmt, ...) { (void)stream; (void)fmt; return 0; }
int fflush(FILE* stream) { (void)stream; return 0; }

// ---- the scheduler seam, cut here (SCOPE 1) ----
//
// The ring never reaches these in this harness: the send path takes them only with a parked
// receiver, and this harness never parks. They exist so `src/fiber_chan.c` links.
neon_fiber* neon_fiber_current(void) { return NULL; }
void neon_fiber_park(void) {}
void neon_fiber_wake(neon_fiber* f) { (void)f; }
neon_fiber* neon_fiber_spawn(neon_fiber_fn fn, void* arg) { (void)fn; (void)arg; return NULL; }

// Distinct addresses, so "the right value came out" is pointer identity.
#define NVAL 11
static int g_val[NVAL];

static void send(neon_chan* ch, int i) { neon_chan_send(ch, &g_val[i]); }

static int recv(neon_chan* ch) {
    void* out = NULL;
    bool got = neon_chan_recv(ch, &out);
    PROVE(got, "a receive on a non-empty channel returns a value");
    // Identify the slot by address: a value the channel invented, or one it read out of a
    // slot nothing wrote, matches none of them.
    for (int i = 1; i < NVAL; i++) {
        if (out == &g_val[i]) {
            return i;
        }
    }
    PROVE(0, "every value received is one this model sent");
    return 0;
}

int main(void) {
    neon_chan* ch = neon_chan_new();

    // ---- phase 1: wrap the READ index ----
    //
    // cap 0 -> 4 on the first send; the ring fills exactly, and draining all four takes
    // head 0,1,2,3 and then back to 0. That last step is the read wrap, and it is the only
    // place in the harness that exercises it — the growth below resets head to 0 long
    // before it could come round again.
    send(ch, 1);
    send(ch, 2);
    send(ch, 3);
    send(ch, 4);
    PROVE(recv(ch) == 1, "the first value out is the first value in");
    PROVE(recv(ch) == 2, "the second value out is the second value in");
    PROVE(recv(ch) == 3, "values come out in order while the ring is not yet wrapped");
    PROVE(recv(ch) == 4, "the read index wraps back to the front of the ring");

    // ---- phase 2: wrap the WRITE index, then grow across the wrap ----
    //
    // Refill from head 0, drain one to push head to 1, then refill: 9 lands at index 0,
    // ahead of head, so the live run is 6,7,8 | 9 rather than 6,7,8,9.
    send(ch, 5);
    send(ch, 6);
    send(ch, 7);
    send(ch, 8);
    PROVE(recv(ch) == 5, "a receive after the wrap still takes the oldest value");
    send(ch, 9);

    // This send finds len == cap == 4 with head == 1, and grows. The copy loop must
    // linearise 6,7,8,9 out of the wrapped ring and reset head to 0.
    send(ch, 10);

    PROVE(recv(ch) == 6, "growth keeps the oldest value oldest");
    PROVE(recv(ch) == 7, "growth preserves the run that sat at the end of the old ring");
    PROVE(recv(ch) == 8, "growth preserves the order up to the wrap boundary");
    PROVE(recv(ch) == 9, "growth preserves the run that had wrapped to the front");
    PROVE(recv(ch) == 10, "a value buffered after the growth follows the ones buffered before");

    // Drained and closed: a receive now reports the channel is done, not a stale slot.
    neon_chan_close(ch);
    void* out = &g_val[0];
    PROVE(!neon_chan_recv(ch, &out), "a closed, drained channel receives nothing");
    PROVE(out == &g_val[0], "a receive that returns false leaves the out-parameter alone");

    neon_chan_free(ch);
    return 0;
}
