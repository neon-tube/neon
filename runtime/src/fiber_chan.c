// Channels and Task/await over the scheduler's park/wake (src/fiber_sched.c). See
// neon/channel.h. Single-thread cooperative (the M=1 milestone): exactly one fiber runs at a
// time, so there are no locks — a send that finds a parked receiver hands the value over and
// wakes it, all without anyone else touching the channel in between. The M:N build adds a
// per-channel lock here; nothing else changes.

#include "libneon_rt.h"

#include "fiber_internal.h" // a task attaches its reap hook to the body fiber
#include "internal.h"       // neon_current_arena, for the rendezvous receiver-arena copy
#include "neon/channel.h"

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

typedef struct neon_channel_waiter {
    neon_fiber* fiber;
    void* slot; // the receiver's out-buffer; a send deep-copies straight into it
    bool delivered;
    struct neon_channel_waiter* next;
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
    // Undelivered values are released here — shared-heap objects, safe from any context.
    if (ch->w->release) {
        for (size_t i = 0; i < ch->len; i++) {
            ch->w->release(ch->buf + ((ch->head + i) % ch->cap) * ch->w->size);
        }
    }
    free(ch->buf);
    neon_free(ch);
}

neon_channel* neon_channel_new(const neon_witness* w) {
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
    return ch;
}

// A bounded channel: sends park once `n` values are buffered, until a receive drains one —
// backpressure, so a fast producer cannot outrun a slow consumer into unbounded memory.
neon_channel* neon_channel_new_bounded(const neon_witness* w, int64_t n) {
    if (n < 1) {
        neon_trap("channel::bounded: the bound must be at least 1");
    }
    neon_channel* ch = neon_channel_new(w);
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

void neon_channel_send(neon_channel* ch, const void* v) {
    if (ch->closed) {
        neon_trap("channel::send: send on a closed channel");
    }
    // Backpressure: on a bounded channel a send parks while the ring is full — and, ON
    // FIRST ENTRY, while earlier senders still wait (FIFO fairness; a late send must not
    // steal the slot a receive just opened for the first in line). Once WOKEN, this sender
    // holds the right-of-way: the receive that woke it popped it off the queue and opened a
    // slot that no later sender can take (they queue behind on entry) — re-checking the
    // queue here would send it to the back of its own line, wasting the wake and stalling
    // the receiver that made room. A close while parked is a trap on wake: the send can
    // never complete, and send-on-closed is already the loud path.
    bool behind = ch->shead != NULL;
    while (ch->bound != 0 && ch->rhead == NULL && (ch->len >= ch->bound || behind)) {
        neon_channel_sender me = {neon_fiber_current(), false, NULL};
        if (ch->stail != NULL) {
            ch->stail->next = &me;
        } else {
            ch->shead = &me;
        }
        ch->stail = &me;
        neon_fiber_park();
        behind = false; // woken by a receive: the opened slot is ours
        if (me.closed) {
            neon_release_shared((neon_header*)ch);
            neon_trap("channel::send: the channel was closed while this send waited");
        }
    }
    // Invariant 2: deep-copy into the destination with allocation routed to the shared
    // heap. The destination is a parked receiver's slot when one is waiting, the ring
    // otherwise.
    void* dst;
    neon_channel_waiter* wake = NULL;
    void* saved;
    if (ch->rhead != NULL) {
        wake = ch->rhead;
        ch->rhead = wake->next;
        if (ch->rhead == NULL) {
            ch->rtail = NULL;
        }
        dst = wake->slot;
        // THE RENDEZVOUS FAST PATH: the receiver is parked, so its arena is exclusively
        // ours for the handoff (parkedness is the lock, even under M:N) — the copy lands
        // directly in the receiver's own heap, single hop, and every copied object is an
        // ordinary arena object with a plain count. This is the original design's
        // "copy into the receiver's arena", and it is what keeps the generic rc path free
        // of atomics.
        saved = neon_current_arena;
        neon_current_arena = wake->fiber->arena;
    } else {
        dst = neon_channel_slot_reserve(ch);
        // Buffered: stage on the plain slab. Ownership is sequential (sender creates, the
        // ring owns, exactly one receiver consumes — the channel lock is the visibility
        // barrier under M:N), so plain rc is sound; the receiver RESTAGES into its own
        // arena at recv.
        saved = neon_send_routing_begin();
    }
    if (ch->w->copy) {
        ch->w->copy(v, dst);
    } else {
        memcpy(dst, v, ch->w->size);
    }
    neon_send_routing_end(saved);
    // Publish only after the copy completed: the deep copy can trap (an unsendable part
    // inside the value), and the killed sender must not leave a half-written slot visible —
    // a ring slot becomes occupied here, a parked receiver is woken here, never before.
    if (wake != NULL) {
        wake->delivered = true;
        neon_fiber_wake(wake->fiber);
    } else {
        ch->len++;
    }
    // Consume the original value (in the SENDER's context, so an arena element frees into
    // its own arena) and the channel reference.
    if (ch->w->release) {
        ch->w->release((void*)v);
    }
    neon_release_shared((neon_header*)ch);
}

bool neon_channel_recv(neon_channel* ch, void* out) {
    bool got;
    if (ch->len > 0) {
        size_t sz = ch->w->size;
        // Restage: deep-copy the staged value into the RECEIVER's own arena (the ambient
        // routing — we are the running receiver), then release the staging copy. Two hops
        // for a buffered value, one for a rendezvous; the price of a generic rc path with
        // no atomics in it.
        void* slot = ch->buf + ch->head * sz;
        if (ch->w->copy) {
            ch->w->copy(slot, out);
        } else {
            memcpy(out, slot, sz);
        }
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
        got = true;
    } else if (ch->closed) {
        got = false;
    } else {
        neon_channel_waiter w = {neon_fiber_current(), out, false, NULL};
        if (ch->rtail != NULL) {
            ch->rtail->next = &w;
        } else {
            ch->rhead = &w;
        }
        ch->rtail = &w;
        neon_fiber_park();
        got = w.delivered;
    }
    // Invariant 3: the handle reference is released only now, after any park — the channel
    // cannot die under a waiter that still names it.
    neon_release_shared((neon_header*)ch);
    return got;
}

void neon_channel_close(neon_channel* ch) {
    if (!ch->closed) {
        ch->closed = true;
        for (neon_channel_waiter* w = ch->rhead; w != NULL;) {
            neon_channel_waiter* next = w->next;
            w->delivered = false;
            neon_fiber_wake(w->fiber);
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
    neon_release_shared((neon_header*)ch);
}

// Whether the channel has been closed. A drained-ness probe belongs to recv (its null);
// this answers "will more values ever arrive" for a receiver whose recv came back empty.
bool neon_channel_is_closed(neon_channel* ch) {
    bool c = ch->closed;
    neon_release_shared((neon_header*)ch);
    return c;
}

// recv with a deadline: true with a value; false when the channel closed-and-drained OR the
// deadline passed first (the caller distinguishes with is_closed if it cares). On a timeout
// the waiter is unlinked HERE — the wake that never came must find nothing to aim at.
bool neon_channel_recv_timeout(neon_channel* ch, void* out, int64_t millis) {
    bool got;
    if (ch->len > 0 || ch->closed) {
        return neon_channel_recv(ch, out); // a value or the drained null, no parking needed
    }
    neon_channel_waiter w = {neon_fiber_current(), out, false, NULL};
    if (ch->rtail != NULL) {
        ch->rtail->next = &w;
    } else {
        ch->rhead = &w;
    }
    ch->rtail = &w;
    if (neon_fiber_park_deadline(millis)) {
        got = w.delivered; // a send (true) or a close (false) won the race
    } else {
        // The deadline fired: pull our waiter out so a later send cannot deliver into a
        // frame that has moved on.
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
        }
        got = false;
    }
    neon_release_shared((neon_header*)ch);
    return got;
}

// The witness `copy` for a channel-handle slot: a channel is a shared-heap identity — two
// handles to it are the same channel — so crossing fibers is a retain and a pointer copy,
// never a deep copy of the channel's contents.
void neon_wcopy_channel(const void* src, void* dst) {
    neon_channel* ch = *(neon_channel* const*)src;
    neon_retain_shared((neon_header*)ch); // a handle's count is concurrent: atomic always
    *(neon_channel**)dst = ch;
}

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
    // The body computes its result IN THIS FIBER'S ARENA; it is then deep-copied to the
    // shared slot and the original released here, in this fiber's own context — exactly a
    // channel send with the task's slot as the destination. The temporary lives on THIS
    // FIBER'S STACK (a VLA), deliberately: a body that traps abandons its stack wholesale,
    // and a heap temporary would leak on exactly that path (LSan found the malloc'd one).
    char tmp[t->w->size];
    shim(body, tmp);
    neon_release(body.env);
    void* saved = neon_send_routing_begin();
    if (t->w->copy) {
        t->w->copy(tmp, (void*)(t + 1));
    } else {
        memcpy((void*)(t + 1), tmp, t->w->size);
    }
    neon_send_routing_end(saved);
    if (t->w->release) {
        t->w->release(tmp);
    }
    t->done = true; // publish strictly after the copy, as a channel send does
    if (t->awaiter != NULL) {
        neon_fiber* a = t->awaiter;
        t->awaiter = NULL;
        neon_fiber_wake(a);
    }
    // The body's task reference is released by the reap hook, which runs for a crash too —
    // the one owner that exists on both outcomes.
}

// The reap hook: how a task learns its body fiber died. On a crash, mark failed and wake
// the awaiter (whose await then propagates); either way, drop the body's task reference.
static void neon_task_lang_reaped(void* arg, bool crashed) {
    neon_task_lang* t = (neon_task_lang*)arg;
    if (crashed && !t->done) {
        t->failed = true;
        if (t->awaiter != NULL) {
            neon_fiber* a = t->awaiter;
            t->awaiter = NULL;
            neon_fiber_wake(a);
        }
    }
    neon_release_shared((neon_header*)t);
}

neon_task_lang* neon_task_lang_spawn(neon_closure body, const neon_witness* w,
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

void neon_task_lang_await(neon_task_lang* t, void* out) {
    if (!t->done && !t->failed) {
        t->awaiter = neon_fiber_current();
        neon_fiber_park(); // woken by the body's completion — or by its crash
    }
    if (t->failed) {
        // Propagate: the awaited work died, so the await dies too — in a fiber that is a
        // clean kill (crash isolation), and failures travel along await edges.
        neon_release_shared((neon_header*)t);
        neon_trap("task::await: the awaited task crashed");
    }
    if (t->taken) {
        neon_trap("task::await: a task's result can be awaited once");
    }
    memcpy(out, (const void*)(t + 1), t->w->size);
    t->taken = true; // the result MOVED out; ownership is the awaiter's now
    neon_release_shared((neon_header*)t);
}

// The witness `copy` for a task-handle slot: a task is a shared-heap identity, exactly as a
// channel is — crossing fibers is a retain and a pointer copy.
void neon_wcopy_task(const void* src, void* dst) {
    neon_task_lang* t = *(neon_task_lang* const*)src;
    neon_retain_shared((neon_header*)t); // a handle's count is concurrent: atomic always
    *(neon_task_lang**)dst = t;
}
