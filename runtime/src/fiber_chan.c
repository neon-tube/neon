// Channels and Task/await over the scheduler's park/wake (src/fiber_sched.c). See
// neon/channel.h. Single-thread cooperative (the M=1 milestone): exactly one fiber runs at a
// time, so there are no locks — a send that finds a parked receiver hands the value over and
// wakes it, all without anyone else touching the channel in between. The M:N build adds a
// per-channel lock here; nothing else changes.

#include "libneon_rt.h"

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

typedef struct neon_channel {
    neon_header header;
    const neon_witness* w;
    char* buf;   // ring of len slots of w->size
    size_t cap;
    size_t head;
    size_t len;
    neon_channel_waiter* rhead;
    neon_channel_waiter* rtail;
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
    void* saved = neon_send_routing_begin(); // invariant 1: the struct itself is shared
    neon_channel* ch =
        (neon_channel*)neon_alloc(sizeof(neon_channel) - sizeof(neon_header), neon_channel_drop);
    neon_send_routing_end(saved);
    ch->w = w;
    ch->buf = NULL;
    ch->cap = 0;
    ch->head = 0;
    ch->len = 0;
    ch->rhead = NULL;
    ch->rtail = NULL;
    ch->closed = false;
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
    // Invariant 2: deep-copy into the destination with allocation routed to the shared
    // heap. The destination is a parked receiver's slot when one is waiting, the ring
    // otherwise.
    void* dst;
    neon_channel_waiter* wake = NULL;
    if (ch->rhead != NULL) {
        wake = ch->rhead;
        ch->rhead = wake->next;
        if (ch->rhead == NULL) {
            ch->rtail = NULL;
        }
        dst = wake->slot;
    } else {
        dst = neon_channel_slot_reserve(ch);
    }
    void* saved = neon_send_routing_begin();
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
    neon_release((neon_header*)ch);
}

bool neon_channel_recv(neon_channel* ch, void* out) {
    bool got;
    if (ch->len > 0) {
        size_t sz = ch->w->size;
        memcpy(out, ch->buf + ch->head * sz, sz);
        ch->head = (ch->head + 1) % ch->cap;
        ch->len--;
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
    neon_release((neon_header*)ch);
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
    }
    neon_release((neon_header*)ch);
}

// The witness `copy` for a channel-handle slot: a channel is a shared-heap identity — two
// handles to it are the same channel — so crossing fibers is a retain and a pointer copy,
// never a deep copy of the channel's contents.
void neon_wcopy_channel(const void* src, void* dst) {
    neon_channel* ch = *(neon_channel* const*)src;
    neon_retain((neon_header*)ch);
    *(neon_channel**)dst = ch;
}
