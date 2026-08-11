// Channels and Task/await over the scheduler's park/wake (src/fiber_sched.c). See
// neon/channel.h. Single-thread cooperative (the M=1 milestone): exactly one fiber runs at a
// time, so there are no locks — a send that finds a parked receiver hands the value over and
// wakes it, all without anyone else touching the channel in between. The M:N build adds a
// per-channel lock here; nothing else changes.

#include "libneon_rt.h"

#include "fiber_internal.h" // a task attaches its reap hook to the body fiber
#include "internal.h"       // neon_current_arena, for the rendezvous receiver-arena copy
#include "neon/channel.h"
#include "neon/list.h" // select_recv walks a List[Channel[T]] of per-holder refs

#include <pthread.h>  // per-channel/per-task locks: the one thing M:N adds here
#include <stdatomic.h> // the select waiter's single atomic claim (see select_recv)
#include <stdint.h>    // uintptr_t, for the canonical body-address lock order
#include <stdlib.h>

// A parked receiver, allocated on the blocked fiber's own stack (which stays live while it is
// parked), so a receive costs no heap. `delivered` distinguishes a value handed over by a
// send from a wake caused by close.
typedef struct neon_chan_waiter {
    neon_fiber* fiber;
    void** slot;
    bool delivered;
    struct neon_chan_waiter* next;
} neon_chan_waiter;

struct neon_chan {
    void** buf;    // ring buffer of buffered values
    size_t cap;    // ring capacity
    size_t head;   // index of the oldest buffered value
    size_t len;    // number buffered
    neon_chan_waiter* rhead; // FIFO of parked receivers
    neon_chan_waiter* rtail;
    bool closed;
};

neon_chan* neon_chan_new(void) {
    neon_chan* ch = calloc(1, sizeof(neon_chan));
    if (ch == NULL) {
        neon_trap("out of memory");
    }
    return ch;
}

static void neon_chan_buffer(neon_chan* ch, void* v) {
    if (ch->len == ch->cap) {
        size_t ncap = ch->cap == 0 ? 4 : ch->cap * 2;
        void** nbuf = malloc(ncap * sizeof(void*));
        if (nbuf == NULL) {
            neon_trap("out of memory");
        }
        for (size_t i = 0; i < ch->len; i++) {
            nbuf[i] = ch->buf[(ch->head + i) % ch->cap];
        }
        free(ch->buf);
        ch->buf = nbuf;
        ch->cap = ncap;
        ch->head = 0;
    }
    ch->buf[(ch->head + ch->len) % ch->cap] = v;
    ch->len++;
}

void neon_chan_send(neon_chan* ch, void* v) {
    if (ch->closed) {
        neon_trap("neon_chan_send: send on a closed channel");
    }
    if (ch->rhead != NULL) {
        // A receiver is parked: hand it the value directly and wake it, skipping the buffer.
        neon_chan_waiter* w = ch->rhead;
        ch->rhead = w->next;
        if (ch->rhead == NULL) {
            ch->rtail = NULL;
        }
        *w->slot = v;
        w->delivered = true;
        neon_fiber_wake(w->fiber);
        return;
    }
    neon_chan_buffer(ch, v);
}

bool neon_chan_recv(neon_chan* ch, void** out) {
    if (ch->len > 0) {
        *out = ch->buf[ch->head];
        ch->head = (ch->head + 1) % ch->cap;
        ch->len--;
        return true;
    }
    if (ch->closed) {
        return false;
    }
    // Park until a send delivers a value (into *out) or a close wakes us empty-handed. The
    // waiter lives on this frame, which persists while we are parked.
    neon_chan_waiter w = {neon_fiber_current(), out, false, NULL};
    if (ch->rtail != NULL) {
        ch->rtail->next = &w;
    } else {
        ch->rhead = &w;
    }
    ch->rtail = &w;
    neon_fiber_park();
    return w.delivered;
}

void neon_chan_close(neon_chan* ch) {
    ch->closed = true;
    for (neon_chan_waiter* w = ch->rhead; w != NULL;) {
        neon_chan_waiter* next = w->next;
        w->delivered = false; // woken by close, not by a value
        neon_fiber_wake(w->fiber);
        w = next;
    }
    ch->rhead = NULL;
    ch->rtail = NULL;
}

void neon_chan_free(neon_chan* ch) {
    if (ch == NULL) {
        return;
    }
    free(ch->buf);
    free(ch);
}

// ---- Task[T] / await ----

struct neon_task {
    void* result;
    bool done;
    neon_fiber* awaiter; // the parked fiber waiting on the result, or NULL
    neon_task_fn fn;
    void* arg;
};

// The fiber a task runs as: produce the result, mark done, and wake the awaiter if one is
// already parked. The task struct is on the global heap, shared with the awaiter — not in
// this fiber's arena, which is dropped the moment this fiber is reaped.
static void neon_task_body(void* targ) {
    neon_task* t = (neon_task*)targ;
    t->result = t->fn(t->arg);
    t->done = true;
    if (t->awaiter != NULL) {
        neon_fiber* a = t->awaiter;
        t->awaiter = NULL;
        neon_fiber_wake(a);
    }
}

neon_task* neon_task_spawn(neon_task_fn fn, void* arg) {
    neon_task* t = calloc(1, sizeof(neon_task));
    if (t == NULL) {
        neon_trap("out of memory");
    }
    t->fn = fn;
    t->arg = arg;
    neon_fiber_spawn(neon_task_body, t);
    return t;
}

void* neon_task_await(neon_task* t) {
    if (!t->done) {
        // Register as the awaiter and park; the task body wakes us when it completes. If it
        // finished already, we skip straight to the result.
        t->awaiter = neon_fiber_current();
        neon_fiber_park();
    }
    return t->result;
}

void neon_task_free(neon_task* t) {
    free(t);
}

// ---- the language channel (`std::channel`'s Channel[T]) ----
//
// Distinct from the `void*` neon_chan above (which the C tests and Task ride): the language
// channel carries VALUES — slots of the element witness's size, like a list's — because a
// Neon value must be deep-copied to cross fibers, not pointed at. Three invariants:
//
//   1. The channel STRUCT lives on the shared heap (allocation routed there at `new`), never
//      in the creating fiber's arena: the handle is retained and released by every fiber
//      that touches it, from any context.
//   2. A sent value is deep-copied through the witness's `copy` with allocation routed to
//      the shared heap (`neon_send_routing_begin/end`), so the copy owes nothing to the
//      sender's arena; the original is then released in the sender's own context.
//   3. Natives consume their arguments (the runtime-wide convention): each op releases the
//      channel handle it was passed — recv only after unparking, so the channel cannot die
//      under a parked receiver holding its reference.

// The shared control block a `select_recv` parks behind — on the select fiber's stack, like
// a waiter, but ONE of it standing behind the N per-channel waiter nodes below. `claimed` is
// the single point of truth for "who won this select": the FIRST channel to deliver a value
// or to close CASes it 0->1 and becomes the winner; every other channel finds it already 1
// and leaves this select untouched. `index`/`delivered` are the winner's report, written
// under its channel's mutex and published before the wake, exactly as a rendezvous value is.
//
// The claim is taken BEFORE the winner's deep copy into the slot — it has to be, or two
// channels could both copy into `out` and corrupt it. The cost is a liveness edge, the same
// class a plain recv already has: if that copy TRAPS (an unsendable part inside the value),
// the sender fiber crashes with the select claimed but never woken, and the select's other
// branches — now locked out by the claim — never fire either. It stems from a sender-side
// crash, already a fatal path under crash isolation, and is the correct trade for
// exactly-once; the alternative (copy then claim) trades a hang for a memory corruption.
typedef struct neon_channel_select {
    _Atomic int claimed; // 0 until a channel wins; CAS to 1 is the claim
    int64_t index;       // which list entry fired (stamped by the winner)
    bool delivered;      // true = a value landed in the slot; false = that channel closed
} neon_channel_select;

typedef struct neon_channel_waiter {
    neon_fiber* fiber;
    void* slot; // the receiver's out-buffer; a send deep-copies straight into it
    bool delivered;
    struct neon_channel_waiter* next;
    // NULL for a plain recv. For a `select_recv` waiter, the shared control this node speaks
    // for, plus the list entry it stands for — so the channel that wins can stamp the index.
    // A plain waiter's positional initializer leaves both zero (select == NULL), which is
    // exactly "not a select", so recv/recv_timeout need no change.
    neon_channel_select* select;
    int64_t sel_index;
} neon_channel_waiter;

// A parked SENDER on a full bounded channel, on its own stack like a receiver's waiter.
// `closed` reports that a close happened while it waited — sending on a closed channel
// traps, and the parked sender must learn that on wake.
typedef struct neon_channel_sender {
    neon_fiber* fiber;
    bool closed;
    struct neon_channel_sender* next;
} neon_channel_sender;

typedef struct neon_channel {
    neon_header header;
    pthread_mutex_t mu; // guards ring + waiter lists + closed; never held across a park
    const neon_witness* w;
    char* buf;   // ring of len slots of w->size
    size_t cap;
    size_t head;
    size_t len;
    size_t bound; // 0 = unbounded; else sends park when len reaches it (backpressure)
    neon_channel_waiter* rhead;
    neon_channel_waiter* rtail;
    neon_channel_sender* shead; // FIFO of parked senders
    neon_channel_sender* stail;
    bool closed;
} neon_channel;

static void neon_channel_drop(void* p) {
    neon_channel* ch = (neon_channel*)p;
    pthread_mutex_destroy(&ch->mu);
    // Undelivered values are released here — shared-heap objects, safe from any context.
    if (ch->w->release) {
        for (size_t i = 0; i < ch->len; i++) {
            ch->w->release(ch->buf + ((ch->head + i) % ch->cap) * ch->w->size);
        }
    }
    free(ch->buf);
    neon_free(ch);
}

static neon_channel* neon_channel_new_impl(const neon_witness* w) {
    void* saved = neon_shared_routing_begin(); // invariant 1: the handle is concurrent-rc
    neon_channel* ch =
        (neon_channel*)neon_alloc(sizeof(neon_channel) - sizeof(neon_header), neon_channel_drop);
    neon_send_routing_end(saved);
    ch->w = w;
    ch->buf = NULL;
    ch->cap = 0;
    ch->head = 0;
    ch->len = 0;
    ch->bound = 0;
    ch->rhead = NULL;
    ch->rtail = NULL;
    ch->shead = NULL;
    ch->stail = NULL;
    ch->closed = false;
    pthread_mutex_init(&ch->mu, NULL);
    return ch;
}

// A bounded channel: sends park once `n` values are buffered, until a receive drains one —
// backpressure, so a fast producer cannot outrun a slow consumer into unbounded memory.
static neon_channel* neon_channel_new_bounded_impl(const neon_witness* w, int64_t n) {
    if (n < 1) {
        neon_trap("channel::bounded: the bound must be at least 1");
    }
    neon_channel* ch = neon_channel_new_impl(w); // the body; the ref layer wraps it
    ch->bound = (size_t)n;
    return ch;
}

static void* neon_channel_slot_reserve(neon_channel* ch) {
    size_t sz = ch->w->size;
    if (ch->len == ch->cap) {
        size_t ncap = ch->cap == 0 ? 4 : ch->cap * 2;
        char* nbuf = malloc(ncap * sz);
        if (nbuf == NULL) {
            neon_trap("out of memory");
        }
        for (size_t i = 0; i < ch->len; i++) {
            memcpy(nbuf + i * sz, ch->buf + ((ch->head + i) % ch->cap) * sz, sz);
        }
        free(ch->buf);
        ch->buf = nbuf;
        ch->cap = ncap;
        ch->head = 0;
    }
    // The caller bumps `len` only after the copy into the slot COMPLETES: a deep copy can
    // trap (an unsendable part), and killing the sending fiber must not leave a half-written
    // slot visible to a receiver.
    return ch->buf + ((ch->head + ch->len) % ch->cap) * sz;
}

// Pop the next DELIVERABLE receiver off ch->rhead. A plain recv waiter comes back as-is. A
// select waiter comes back only when THIS channel wins its select — the atomic claim decides
// — and a select already claimed by ANOTHER channel is unlinked and discarded here (a stale
// node the winning select fiber would otherwise trip over; it will find it gone in its unlink
// walk). Returns NULL once the list holds no live receiver — so a send that reaches the
// backpressure test below has honestly emptied the queue of stale selects first. Caller holds
// ch->mu. The winner's index is stamped here; its `delivered` is set by the caller (a send
// hands over a value, a close does not), which is why this does not touch it.
static neon_channel_waiter* neon_channel_pop_receiver(neon_channel* ch) {
    while (ch->rhead != NULL) {
        neon_channel_waiter* w = ch->rhead;
        ch->rhead = w->next;
        if (ch->rhead == NULL) {
            ch->rtail = NULL;
        }
        w->next = NULL; // detached from this channel's list
        if (w->select == NULL) {
            return w; // an ordinary recv waiter: always deliverable
        }
        int zero = 0;
        if (atomic_compare_exchange_strong(&w->select->claimed, &zero, 1)) {
            w->select->index = w->sel_index; // this channel won the race
            return w;
        }
        // Lost the claim: another channel already delivered to this select. Drop the stale
        // node — we unlinked it above, and the select fiber's own walk tolerates its absence.
    }
    return NULL;
}

// Drain one buffered value into `out`: restage the staged copy into the receiver's own arena
// (ambient routing — the caller is the running receiver), release the staging copy, advance
// the ring, and hand the opened slot to the first parked sender. Caller holds ch->mu and has
// established ch->len > 0. Shared by recv and by select's ready-value path so the two-hop
// restage lives in exactly one place.
static void neon_channel_drain_locked(neon_channel* ch, void* out) {
    size_t sz = ch->w->size;
    void* slot = ch->buf + ch->head * sz;
    // The transfer edge: the staged owning ref demotes, the ref minted here (in OUR arena)
    // takes the baton, and this fiber's first use claims the body.
    neon_transfer_begin();
    if (ch->w->copy) {
        ch->w->copy(slot, out);
    } else {
        memcpy(out, slot, sz);
    }
    neon_transfer_end();
    if (ch->w->release) {
        ch->w->release(slot);
    }
    ch->head = (ch->head + 1) % ch->cap;
    ch->len--;
    if (ch->shead != NULL) {
        // A slot opened: the first parked sender gets its turn.
        neon_channel_sender* sndr = ch->shead;
        ch->shead = sndr->next;
        if (ch->shead == NULL) {
            ch->stail = NULL;
        }
        neon_fiber_wake(sndr->fiber);
    }
}

// False when the channel is (or becomes) closed — the caller's value is UNTOUCHED on that
// path, which is the guarantee std::channel's ClosedError rides on: a send that throws did
// not send, and for a resource that means the baton never left. Both closed exits sit
// strictly before the copy.
static bool neon_channel_send_impl(neon_channel* ch, const void* v) {
    pthread_mutex_lock(&ch->mu);
    if (ch->closed) {
        pthread_mutex_unlock(&ch->mu);
        // Consume `v` all the same — natives consume their arguments, and the caller
        // retained before the call for anything it still holds. Releasing a resource ref
        // here is NOT a transfer: no bracket ran, the caller's own ref keeps the baton.
        if (ch->w->release) {
            ch->w->release((void*)v);
        }
        return false;
    }
    // Backpressure: on a bounded channel a send parks while the ring is full — and, ON
    // FIRST ENTRY, while earlier senders still wait (FIFO fairness; a late send must not
    // steal the slot a receive just opened for the first in line). Once WOKEN, this sender
    // holds the right-of-way: the receive that woke it popped it off the queue and opened a
    // slot that no later sender can take (they queue behind on entry) — so `right_of_way`
    // skips the backpressure test entirely, where re-testing would send it to the back of
    // its own line and stall the receiver that made room. A close while parked is a trap on
    // wake: the send can never complete, and send-on-closed is already the loud path.
    //
    // The delivery and the backpressure test share one loop so that pop_receiver runs FIRST:
    // it drains any stale select waiter (one claimed by another channel, not yet lazily
    // unlinked) off the head, so a NULL from it means the receiver list is genuinely empty
    // and the bounded test that follows is honest — a stale select cannot masquerade as a
    // receiver and wave a full channel past its bound.
    bool right_of_way = false;
    for (;;) {
        neon_channel_waiter* wake = neon_channel_pop_receiver(ch);
        if (wake != NULL) {
            // THE RENDEZVOUS FAST PATH: the receiver is parked, so its arena is exclusively
            // ours for the handoff (parkedness is the lock, even under M:N) — the copy lands
            // directly in the receiver's own heap, single hop, and every copied object is an
            // ordinary arena object with a plain count. This is the original design's
            // "copy into the receiver's arena", and it is what keeps the generic rc path
            // free of atomics.
            void* dst = wake->slot;
            void* saved = neon_current_arena;
            neon_current_arena = wake->fiber->arena;
            // A channel send is a TRANSFER EDGE: any owning resource ref inside the value
            // moves its baton (neon_wcopy_resource, reached through the witness walk). The
            // bracket wraps exactly the copy, on this thread, before publication — so a
            // woken receiver can only ever observe a completed handoff.
            neon_transfer_begin();
            if (ch->w->copy) {
                ch->w->copy(v, dst);
            } else {
                memcpy(dst, v, ch->w->size);
            }
            neon_transfer_end();
            neon_send_routing_end(saved);
            // Publish only after the copy completed: the deep copy can trap (an unsendable
            // part inside the value), and the killed sender must not leave a half-written
            // slot visible — the receiver is woken here, never before. For a select waiter
            // the value is what it woke for, so its control's `delivered` is true (the
            // winning index was already stamped by the claim in pop_receiver).
            wake->delivered = true;
            if (wake->select != NULL) {
                wake->select->delivered = true;
            }
            neon_fiber_wake(wake->fiber);
            pthread_mutex_unlock(&ch->mu);
            // Consume the original value (in the SENDER's context, so an arena element frees
            // into its own arena) and, at the ref layer, the channel reference. For a sent
            // resource this releases a now-demoted ref — its drop is inert, the staged ref
            // carries the baton.
            if (ch->w->release) {
                ch->w->release((void*)v);
            }
            return true;
        }
        // No receiver waiting. On a bounded channel, park until a receive opens a slot,
        // unless we already hold the right-of-way from a prior wake.
        if (!right_of_way && ch->bound != 0 && (ch->len >= ch->bound || ch->shead != NULL)) {
            neon_channel_sender me = {neon_fiber_current(), false, NULL};
            if (ch->stail != NULL) {
                ch->stail->next = &me;
            } else {
                ch->shead = &me;
            }
            ch->stail = &me;
            pthread_mutex_unlock(&ch->mu);
            neon_fiber_park();
            pthread_mutex_lock(&ch->mu);
            if (me.closed) {
                pthread_mutex_unlock(&ch->mu);
                if (ch->w->release) { // consume, as above; no transfer happened
                    ch->w->release((void*)v);
                }
                return false;
            }
            right_of_way = true; // the receive that woke us opened a slot that is ours
            continue;
        }
        // Buffered: stage on the plain slab. Ownership is sequential (sender creates, the
        // ring owns, exactly one receiver consumes — the channel lock is the visibility
        // barrier under M:N), so plain rc is sound; the receiver RESTAGES into its own arena
        // at recv. Same transfer edge as the rendezvous copy above.
        void* dst = neon_channel_slot_reserve(ch);
        void* saved = neon_send_routing_begin();
        neon_transfer_begin();
        if (ch->w->copy) {
            ch->w->copy(v, dst);
        } else {
            memcpy(dst, v, ch->w->size);
        }
        neon_transfer_end();
        neon_send_routing_end(saved);
        ch->len++; // a ring slot becomes occupied only after the copy completed
        pthread_mutex_unlock(&ch->mu);
        if (ch->w->release) {
            ch->w->release((void*)v);
        }
        return true;
    }
}

static bool neon_channel_recv_impl(neon_channel* ch, void* out) {
    bool got;
    pthread_mutex_lock(&ch->mu);
    if (ch->len > 0) {
        // Restage a buffered value into our own arena and open a slot for a parked sender.
        // Two hops for a buffered value, one for a rendezvous; the price of a generic rc
        // path with no atomics in it.
        neon_channel_drain_locked(ch, out);
        got = true;
    } else if (ch->closed) {
        got = false;
    } else {
        neon_channel_waiter w = {neon_fiber_current(), out, false, NULL, NULL, 0};
        if (ch->rtail != NULL) {
            ch->rtail->next = &w;
        } else {
            ch->rhead = &w;
        }
        ch->rtail = &w;
        pthread_mutex_unlock(&ch->mu);
        neon_fiber_park();
        got = w.delivered;
        goto out_unlocked;
    }
    pthread_mutex_unlock(&ch->mu);
out_unlocked:
    return got;
}

static void neon_channel_close_impl(neon_channel* ch) {
    pthread_mutex_lock(&ch->mu);
    if (!ch->closed) {
        ch->closed = true;
        for (neon_channel_waiter* w = ch->rhead; w != NULL;) {
            neon_channel_waiter* next = w->next;
            if (w->select == NULL) {
                w->delivered = false;
                neon_fiber_wake(w->fiber);
            } else {
                // A parked select: this close is a candidate winner. Claim it (CAS 0->1);
                // the winner reports this channel's index with the drained-null outcome. A
                // lost claim means another channel already delivered to that select first —
                // leave it be, its wake stands and this node is a stale one its unlink walk
                // will drop.
                int zero = 0;
                if (atomic_compare_exchange_strong(&w->select->claimed, &zero, 1)) {
                    w->select->index = w->sel_index;
                    w->select->delivered = false; // woken by close, not by a value
                    neon_fiber_wake(w->fiber);
                }
            }
            w = next;
        }
        ch->rhead = NULL;
        ch->rtail = NULL;
        for (neon_channel_sender* s = ch->shead; s != NULL;) {
            neon_channel_sender* next = s->next;
            s->closed = true; // the woken send traps: it can never complete
            neon_fiber_wake(s->fiber);
            s = next;
        }
        ch->shead = NULL;
        ch->stail = NULL;
    }
    pthread_mutex_unlock(&ch->mu);
}

// Whether the channel has been closed. A drained-ness probe belongs to recv (its null);
// this answers "will more values ever arrive" for a receiver whose recv came back empty.
static bool neon_channel_is_closed_impl(neon_channel* ch) {
    pthread_mutex_lock(&ch->mu);
    bool c = ch->closed;
    pthread_mutex_unlock(&ch->mu);
    return c;
}

// recv with a deadline: true with a value; false when the channel closed-and-drained OR the
// deadline passed first (the caller distinguishes with is_closed if it cares). On a timeout
// the waiter is unlinked HERE — the wake that never came must find nothing to aim at.
static bool neon_channel_recv_timeout_impl(neon_channel* ch, void* out, int64_t millis) {
    bool got;
    pthread_mutex_lock(&ch->mu);
    if (ch->len > 0 || ch->closed) {
        pthread_mutex_unlock(&ch->mu);
        return neon_channel_recv_impl(ch, out); // a value or the drained null, no parking
    }
    neon_channel_waiter w = {neon_fiber_current(), out, false, NULL, NULL, 0};
    if (ch->rtail != NULL) {
        ch->rtail->next = &w;
    } else {
        ch->rhead = &w;
    }
    ch->rtail = &w;
    pthread_mutex_unlock(&ch->mu);
    if (neon_fiber_park_deadline(millis)) {
        got = w.delivered; // a send (true) or a close (false) won the race
    } else {
        // The deadline fired. Under the LOCK, either our waiter is still linked — a true
        // timeout, and unlinking it is our claim — or a send/close claimed it in the race
        // window and its delivery stands (the wake it also issued dissolves against the
        // queued bit; the admission the deadline made is the one that ran us).
        pthread_mutex_lock(&ch->mu);
        neon_channel_waiter** link = &ch->rhead;
        neon_channel_waiter* prev = NULL;
        while (*link != NULL && *link != &w) {
            prev = *link;
            link = &(*link)->next;
        }
        if (*link == &w) {
            *link = w.next;
            if (ch->rtail == &w) {
                ch->rtail = prev;
            }
            got = false;
        } else {
            got = w.delivered; // claimed by a send in the window: the value is ours
        }
        pthread_mutex_unlock(&ch->mu);
    }
    return got;
}

// ---- select: receive from whichever of several channels is ready first ----
//
// Block until ONE of N channels can hand over a value (or is closed-and-drained), take from
// exactly that one, and leave the others untouched. It is recv's park/wake, widened from one
// channel to N — with three problems N brings that one did not:
//
//   1. DEADLOCK. A select spans N per-channel mutexes, and two selects sharing channels in
//      opposite orders would deadlock. So the locks are always taken in a canonical order —
//      by body ADDRESS — and duplicates (the same channel listed twice) are locked once.
//   2. EXACTLY-ONE DELIVERY. The waiter is linked into N receiver lists at once, so N senders
//      could each try to hand it a value. A single atomic `claimed` on the shared control is
//      the arbiter: the first channel to claim it (CAS 0->1) delivers; every other finds it
//      already claimed and skips (neon_channel_pop_receiver / close's claim). One winner, one
//      value, by construction.
//   3. CLEANUP. The winner unlinks its own node when it pops it; the losers never touched
//      theirs; a channel that skipped a stale node unlinked it too. On wake the select fiber
//      walks all N channels and unlinks whatever remains, so every node is gone before the
//      stack frame holding them dies — the crash-locals discipline recv's on-stack waiter
//      follows, scaled to N.
//
// The waiter nodes, their bodies, and the sort scratch are VLAs on the select fiber's stack:
// they die with the frame, no heap to leak on a mid-copy trap — the same choice the task body
// makes for its result temporary, for the same reason. Returns true with a value in `out` and
// the winning list index in `*out_index`; false (the drained-null outcome) with the index of
// the closed channel. Priority: a ready VALUE anywhere beats a closed channel, and ties go to
// the lowest list index — so a closed channel in the set never masks data still waiting, and
// the outcome is deterministic.

// Lock every DISTINCT body once, in the canonical (address) order `order` is pre-sorted into;
// duplicates sit adjacent after the sort, so skipping equal-to-previous locks each body once.
static void neon_channel_select_lock_all(neon_channel** order, size_t n) {
    for (size_t i = 0; i < n; i++) {
        if (i > 0 && order[i] == order[i - 1]) {
            continue; // a duplicate body, already locked
        }
        pthread_mutex_lock(&order[i]->mu);
    }
}
static void neon_channel_select_unlock_all(neon_channel** order, size_t n) {
    for (size_t i = 0; i < n; i++) {
        if (i > 0 && order[i] == order[i - 1]) {
            continue;
        }
        pthread_mutex_unlock(&order[i]->mu);
    }
}

// The impl itself derefs each list entry to its body, so it lives in the handle layer below,
// past the neon_channel_ref definition; the public neon_channel_select_recv wrapper is beside
// it.



// ---- the language task (`std::task`'s Task[T]) ----
//
// A task is a fiber whose RESULT crosses back: the body runs in its own fiber (own arena),
// and the value it returns is deep-copied to the shared heap before the awaiter sees it —
// the same copy-on-send discipline as a channel, for the same reason. The handle is a
// shared-heap refcounted struct with the result slot inline after it; `await` is one-shot
// (the result moves out to the awaiter). A task body that traps never completes, and its
// awaiter parks forever — which the scheduler then reports as a deadlock rather than
// hanging: honest, if blunt, until structured failure propagation exists.

typedef struct neon_task_lang {
    neon_header header;
    pthread_mutex_t mu; // guards done/failed/awaiter across completion, reap, await
    const neon_witness* w;
    bool done;
    bool failed;         // the body fiber CRASHED; await propagates rather than parking forever
    bool taken;          // the result moved out to an awaiter; drop must not release it
    neon_fiber* awaiter; // parked awaiter, woken at completion
    // the result bytes follow, w->size of them
} neon_task_lang;

// The per-repr call shim, emitted by codegen: call the body closure (zero-arg, returning
// the result type only it knows) and write the result to `out`.
typedef void (*neon_task_shim)(neon_closure f, void* out);

typedef struct {
    neon_task_lang* task; // holds one reference for the body fiber's lifetime
    neon_closure body;
    neon_task_shim shim;
} neon_task_cell;

static void neon_task_lang_drop(void* p) {
    neon_task_lang* t = (neon_task_lang*)p;
    pthread_mutex_destroy(&t->mu);
    if (t->done && !t->taken && t->w->release) {
        t->w->release((void*)(t + 1)); // a result nobody awaited — shared-heap, safe anywhere
    }
    neon_free(t);
}

static void neon_task_lang_body(void* arg) {
    neon_task_cell* cell = (neon_task_cell*)arg;
    neon_task_lang* t = cell->task;
    neon_closure body = cell->body;
    neon_task_shim shim = cell->shim;
    free(cell);
    // Park the env for crash teardown, exactly as neon_fiber_run_closure does: a body
    // that traps abandons this frame, the env is off-arena (shared heap), and a moved-in
    // resource's owning ref would leak with it — an fd whose cleanup never runs.
    neon_fiber* self_f = neon_fiber_current();
    if (self_f != NULL) {
        self_f->body_env = body.env;
    }
    // The body computes its result IN THIS FIBER'S ARENA; it is then deep-copied to the
    // shared slot and the original released here, in this fiber's own context — exactly a
    // channel send with the task's slot as the destination. The temporary lives on THIS
    // FIBER'S STACK (a VLA), deliberately: a body that traps abandons its stack wholesale,
    // and a heap temporary would leak on exactly that path (LSan found the malloc'd one).
    char tmp[t->w->size];
    shim(body, tmp);
    if (self_f != NULL) {
        self_f->body_env = NULL; // normal exit: this frame does the release
    }
    neon_release(body.env);
    // A task RETURN is a transfer edge: a resource in the result hands its baton to the
    // staged ref, which await's restage below hands on to the awaiter.
    void* saved = neon_send_routing_begin();
    neon_transfer_begin();
    if (t->w->copy) {
        t->w->copy(tmp, (void*)(t + 1));
    } else {
        memcpy((void*)(t + 1), tmp, t->w->size);
    }
    neon_transfer_end();
    neon_send_routing_end(saved);
    if (t->w->release) {
        t->w->release(tmp);
    }
    pthread_mutex_lock(&t->mu);
    t->done = true; // publish strictly after the copy, as a channel send does
    neon_fiber* a = t->awaiter;
    t->awaiter = NULL;
    pthread_mutex_unlock(&t->mu);
    if (a != NULL) {
        neon_fiber_wake(a);
    }
    // The body's task reference is released by the reap hook, which runs for a crash too —
    // the one owner that exists on both outcomes.
}

// The reap hook: how a task learns its body fiber died. On a crash, mark failed and wake
// the awaiter (whose await then propagates); either way, drop the body's task reference.
static void neon_task_lang_reaped(void* arg, bool crashed) {
    neon_task_lang* t = (neon_task_lang*)arg;
    pthread_mutex_lock(&t->mu);
    neon_fiber* a = NULL;
    if (crashed && !t->done) {
        t->failed = true;
        a = t->awaiter;
        t->awaiter = NULL;
    }
    pthread_mutex_unlock(&t->mu);
    if (a != NULL) {
        neon_fiber_wake(a);
    }
    neon_release_shared((neon_header*)t);
}

static neon_task_lang* neon_task_lang_spawn_impl(neon_closure body, const neon_witness* w,
                                                 neon_task_shim shim) {
    // A capturing body crosses like any spawn's: an arena environment is deep-copied to
    // the shared heap through the env-copy table.
    if (body.env != NULL && (body.env->flags & NEON_ALLOC_ARENA)) {
        body.env = neon_env_copy_to_shared(body.env);
    }
    void* saved = neon_shared_routing_begin(); // the handle is concurrent-rc, like a channel
    neon_task_lang* t = (neon_task_lang*)neon_alloc(
        sizeof(neon_task_lang) - sizeof(neon_header) + w->size, neon_task_lang_drop);
    neon_send_routing_end(saved);
    t->w = w;
    t->done = false;
    t->failed = false;
    t->taken = false;
    t->awaiter = NULL;
    pthread_mutex_init(&t->mu, NULL);
    neon_task_cell* cell = malloc(sizeof(neon_task_cell));
    if (cell == NULL) {
        neon_trap("out of memory");
    }
    neon_retain_shared((neon_header*)t); // the body fiber's reference, released by the reap hook
    cell->task = t;
    cell->body = body;
    cell->shim = shim;
    neon_fiber* fb = neon_fiber_spawn(neon_task_lang_body, cell);
    fb->on_reap = neon_task_lang_reaped;
    fb->on_reap_arg = t;
    return t;
}

static void neon_task_lang_await_impl(neon_task_lang* t, void* out) {
    pthread_mutex_lock(&t->mu);
    if (!t->done && !t->failed) {
        t->awaiter = neon_fiber_current();
        pthread_mutex_unlock(&t->mu);
        neon_fiber_park(); // woken by the body's completion — or by its crash
        pthread_mutex_lock(&t->mu);
    }
    // failed/taken are decided UNDER THE LOCK: task handles cross fibers, so two awaiters
    // on different seats can race here, and an unlocked taken-check let both restage —
    // and both release the staged slot, a double free. The winner claims `taken` inside
    // the lock and restages after unlocking (a deep copy can trap, and a trap must not
    // abandon the mutex); the loser traps.
    bool failed = t->failed;
    bool lost = !failed && t->taken;
    if (!failed && !lost) {
        t->taken = true; // claimed: the staged slot is exclusively ours now
    }
    pthread_mutex_unlock(&t->mu);
    if (failed) {
        // Propagate: the awaited work died, so the await dies too — in a fiber that is a
        // clean kill (crash isolation), and failures travel along await edges.
        neon_trap("task::await: the awaited task crashed");
    }
    if (lost) {
        neon_trap("task::await: a task's result can be awaited once");
    }
    // RESTAGE, exactly as a channel recv does: deep-copy the staged result into the
    // AWAITER's own arena (the ambient routing — we are the running awaiter), release the
    // staging copy, and only then mark it taken. The old `memcpy` handoff left the result
    // slab-resident, invisible to the awaiter's crash-teardown walk — a leak for every
    // result type, and with resources it would have been an fd whose cleanup never runs.
    // Also a transfer edge: the staged owning ref hands the baton to the ref minted here.
    neon_transfer_begin();
    if (t->w->copy) {
        t->w->copy((const void*)(t + 1), out);
    } else {
        memcpy(out, (const void*)(t + 1), t->w->size);
    }
    neon_transfer_end();
    if (t->w->release) {
        t->w->release((void*)(t + 1)); // the staged slot is dead bytes from here
    }
}



// ---- the handle layer: why a fiber's channel is TWO objects ----
//
// A channel has a shared BODY (the ring, the waiter lists, the lock) that genuinely lives
// across fibers and threads, and a per-holder REF — an ordinary object in the holder's own
// heap that owns exactly one reference to that body. The Neon value `Channel[T]` is the
// ref; every operation derefs to the body.
//
// The split exists to make one bug UNREPRESENTABLE rather than tracked. Before it, a
// handle was a bare pointer to the shared body, so a handle held only in a crashed fiber's
// LOCALS — never stored in any arena object — was invisible to the teardown walk and
// leaked (the body, and everything buffered in it). Locals die with the abandoned stack
// and there are no stack maps to find them; conservative stack scanning is unsound for a
// refcount release (a stale word that looks like a pointer would double-release). Making
// the handle arena-RESIDENT solves it structurally: the walk finds every live ref by
// construction, and the ref's drop releases the body. Nothing tracks anything.
//
// It also simplifies the counting. A ref is an ordinary arena object with a plain,
// thread-local count — so codegen emits the generic retain/release for handles again, and
// the atomic pair is confined to this file, where a ref's birth and death touch the body.
// Refs never cross threads: a handle crossing fibers is a COPY (below), which mints a
// fresh ref in the destination's heap. Same shape as `neon_str`'s data + owner.

typedef struct neon_channel_ref {
    neon_header header;
    neon_channel* body;
} neon_channel_ref;

static void neon_channel_ref_drop(void* p) {
    neon_channel_ref* r = (neon_channel_ref*)p;
    neon_release_shared((neon_header*)r->body); // the body's count IS concurrent
    neon_free(r);
}

// A handle in the CURRENT context's heap (the running fiber's arena, or the slab off-fiber)
// owning the reference the caller already holds on `body`.
static neon_channel_ref* neon_channel_ref_new(neon_channel* body) {
    neon_channel_ref* r = (neon_channel_ref*)neon_alloc(
        sizeof(neon_channel_ref) - sizeof(neon_header), neon_channel_ref_drop);
    r->body = body;
    return r;
}

neon_channel_ref* neon_channel_new(const neon_witness* w) {
    return neon_channel_ref_new(neon_channel_new_impl(w));
}

neon_channel_ref* neon_channel_new_bounded(const neon_witness* w, int64_t n) {
    return neon_channel_ref_new(neon_channel_new_bounded_impl(w, n));
}

// Each op borrows the body for the duration and then consumes the caller's REF — the
// runtime-wide native convention, now paid on an ordinary arena object.
bool neon_channel_send(neon_channel_ref* r, const void* v) {
    bool sent = neon_channel_send_impl(r->body, v);
    neon_release((neon_header*)r);
    return sent;
}

bool neon_channel_recv(neon_channel_ref* r, void* out) {
    bool got = neon_channel_recv_impl(r->body, out);
    neon_release((neon_header*)r);
    return got;
}

bool neon_channel_recv_timeout(neon_channel_ref* r, void* out, int64_t millis) {
    bool got = neon_channel_recv_timeout_impl(r->body, out, millis);
    neon_release((neon_header*)r);
    return got;
}

void neon_channel_close(neon_channel_ref* r) {
    neon_channel_close_impl(r->body);
    neon_release((neon_header*)r);
}

bool neon_channel_is_closed(neon_channel_ref* r) {
    bool c = neon_channel_is_closed_impl(r->body);
    neon_release((neon_header*)r);
    return c;
}

// select_recv's body: see the block near the top of this file for the claim/lock-order/
// cleanup argument. Here because it derefs each list entry's ref to its shared body.
static bool neon_channel_select_recv_impl(neon_list* list, void* out, int64_t* out_index) {
    size_t n = (size_t)list->len;
    if (n == 0) {
        // A select over nothing has nothing to wake it: refuse loudly rather than park a
        // fiber the scheduler would then report as a deadlock.
        neon_trap("channel::select_recv: the channel list is empty");
    }
    neon_channel* bodies[n]; // the body behind each list entry, by list index
    neon_channel* order[n];  // the same bodies, sorted by address for lock ordering
    neon_channel_waiter waiters[n];
    for (size_t i = 0; i < n; i++) {
        neon_channel_ref* ref = *(neon_channel_ref* const*)neon_list_at(list, (int64_t)i);
        bodies[i] = ref->body;
        order[i] = ref->body;
    }
    // Insertion sort `order` by address (a select's arity is small; this is not a hot path).
    for (size_t i = 1; i < n; i++) {
        neon_channel* key = order[i];
        size_t j = i;
        while (j > 0 && (uintptr_t)order[j - 1] > (uintptr_t)key) {
            order[j] = order[j - 1];
            j--;
        }
        order[j] = key;
    }
    neon_channel_select sel = {0, -1, false};

    neon_channel_select_lock_all(order, n);
    // Under all the locks, a consistent snapshot: a ready value first (lowest index wins),
    // then failing that a closed channel. Holding every lock is what makes the decision and
    // the registration below atomic — no send can slip a value in between "nothing ready" and
    // "parked", the lost-wakeup every such design has to close.
    for (size_t i = 0; i < n; i++) {
        if (bodies[i]->len > 0) {
            neon_channel_drain_locked(bodies[i], out);
            neon_channel_select_unlock_all(order, n);
            *out_index = (int64_t)i;
            return true;
        }
    }
    for (size_t i = 0; i < n; i++) {
        if (bodies[i]->closed) { // len == 0 here — the value scan above found none
            neon_channel_select_unlock_all(order, n);
            *out_index = (int64_t)i;
            return false; // closed-and-drained: the null outcome
        }
    }
    // Nothing ready: link a receiver waiter onto every channel, then park. Each waiter points
    // at the one shared `sel`; the first channel to deliver or close claims it and reports
    // through it. Duplicate bodies get one waiter per list entry, both on that one list.
    for (size_t i = 0; i < n; i++) {
        neon_channel_waiter* w = &waiters[i];
        w->fiber = neon_fiber_current();
        w->slot = out;
        w->delivered = false;
        w->next = NULL;
        w->select = &sel;
        w->sel_index = (int64_t)i;
        neon_channel* ch = bodies[i];
        if (ch->rtail != NULL) {
            ch->rtail->next = w;
        } else {
            ch->rhead = w;
        }
        ch->rtail = w;
    }
    neon_channel_select_unlock_all(order, n);
    neon_fiber_park();
    // Woken: exactly one channel claimed `sel` — its value already sits in `out` (rendezvous
    // copy, into our arena) or it closed. Unlink our node from every channel it may still be
    // on. Each iteration locks one body, unlinks one waiter, unlocks — never nested, so a
    // duplicate body (locked, unlocked, re-locked) cannot deadlock against itself, and no
    // canonical order is needed here.
    for (size_t i = 0; i < n; i++) {
        neon_channel* ch = bodies[i];
        pthread_mutex_lock(&ch->mu);
        neon_channel_waiter** link = &ch->rhead;
        neon_channel_waiter* prev = NULL;
        while (*link != NULL) {
            if (*link == &waiters[i]) {
                *link = waiters[i].next;
                if (ch->rtail == &waiters[i]) {
                    ch->rtail = prev;
                }
                break;
            }
            prev = *link;
            link = &(*link)->next;
        }
        pthread_mutex_unlock(&ch->mu);
    }
    *out_index = sel.index;
    return sel.delivered;
}

// select_recv borrows every channel body through the list for the duration, then consumes
// the LIST — releasing it drops each per-holder ref it carries, exactly the native-consume
// convention paid once on the aggregate. Released only after the impl returns, so no body can
// die under a still-parked select waiter that references it.
bool neon_channel_select_recv(neon_list* list, void* out, int64_t* out_index) {
    bool got = neon_channel_select_recv_impl(list, out, out_index);
    neon_release((neon_header*)list);
    return got;
}

// The witness `copy` for a handle slot: a channel is an identity, so crossing fibers keeps
// the SAME body — but mints a fresh ref in the destination's heap (the ambient routing),
// because a ref belongs to exactly one holder. That is what keeps a ref's count
// thread-local while the body's is atomic.
void neon_wcopy_channel(const void* src, void* dst) {
    neon_channel_ref* r = *(neon_channel_ref* const*)src;
    neon_retain_shared((neon_header*)r->body);
    *(neon_channel_ref**)dst = neon_channel_ref_new(r->body);
}

// ---- the same split for tasks ----

typedef struct neon_task_ref {
    neon_header header;
    neon_task_lang* body;
} neon_task_ref;

static void neon_task_ref_drop(void* p) {
    neon_task_ref* r = (neon_task_ref*)p;
    neon_release_shared((neon_header*)r->body);
    neon_free(r);
}

static neon_task_ref* neon_task_ref_new(neon_task_lang* body) {
    neon_task_ref* r = (neon_task_ref*)neon_alloc(
        sizeof(neon_task_ref) - sizeof(neon_header), neon_task_ref_drop);
    r->body = body;
    return r;
}

neon_task_ref* neon_task_lang_spawn(neon_closure body, const neon_witness* w,
                                    neon_task_shim shim) {
    return neon_task_ref_new(neon_task_lang_spawn_impl(body, w, shim));
}

void neon_task_lang_await(neon_task_ref* r, void* out) {
    neon_task_lang_await_impl(r->body, out);
    neon_release((neon_header*)r);
}

void neon_wcopy_task(const void* src, void* dst) {
    neon_task_ref* r = *(neon_task_ref* const*)src;
    neon_retain_shared((neon_header*)r->body);
    *(neon_task_ref**)dst = neon_task_ref_new(r->body);
}
