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
