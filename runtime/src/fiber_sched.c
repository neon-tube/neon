// The cooperative fiber scheduler — a run queue over the swap primitive in fiber.c. This is
// the M=1 first milestone of the design's M:N target (docs/design/fibers.md): one OS thread,
// a FIFO of ready fibers, run each until it yields or finishes. The queue is deliberately
// behind neon_fiber_runtime/spawn/sched_yield so that turning it into a per-thread
// work-stealing deque later is a change here, not at every call site.
//
// The scheduler runs on the thread's root context — resuming a fiber returns to this loop
// when the fiber yields or finishes — so there is no separate scheduler stack, and this same
// loop is what each worker OS thread would run in the M:N build.

#include "libneon_rt.h"

#include "fiber_internal.h"
#include "internal.h" // neon_fiber_trap_handler

#include <stddef.h>

// One scheduler per OS thread. `active` guards against nesting a runtime inside a fiber and
// tells spawn there is a queue to add to.
typedef struct {
    neon_fiber* head;
    neon_fiber* tail;
    bool active;
} neon_sched;

static _Thread_local neon_sched t_sched;

static void neon_sched_enqueue(neon_fiber* f) {
    f->q_next = NULL;
    f->state = NEON_FIBER_READY;
    if (t_sched.tail != NULL) {
        t_sched.tail->q_next = f;
    } else {
        t_sched.head = f;
    }
    t_sched.tail = f;
}

static neon_fiber* neon_sched_dequeue(void) {
    neon_fiber* f = t_sched.head;
    if (f != NULL) {
        t_sched.head = f->q_next;
        if (t_sched.head == NULL) {
            t_sched.tail = NULL;
        }
        f->q_next = NULL;
    }
    return f;
}

void neon_fiber_runtime(neon_fiber_fn body, void* arg) {
    if (t_sched.active) {
        neon_trap("neon_fiber_runtime: a scheduler is already running on this thread");
    }
    t_sched.active = true;

    neon_sched_enqueue(neon_fiber_new(body, arg, 0));

    // Pump until the queue drains. A fiber that finishes is freed (its arena bulk-dropped); a
    // fiber that yielded cooperatively is re-enqueued to run again. Blocking on a channel —
    // where a fiber leaves the queue without finishing and is woken later — is slice 5; here
    // the only two outcomes are finished and yielded.
    neon_fiber* f;
    while ((f = neon_sched_dequeue()) != NULL) {
        f->state = NEON_FIBER_RUNNING;
        // Arm the trap→fiber bridge while a fiber runs (it, or any fiber it nests into, may
        // trap), and disarm the moment we are back on the root context so that a trap in the
        // scheduler's own code (an allocation OOM here) stays fatal rather than trying to
        // "kill a fiber" that is really the root. A crashed fiber comes back through the same
        // resume return as a finished one, flagged crashed.
        neon_fiber_trap_handler = neon_fiber_on_trap;
        neon_fiber_resume(f);
        neon_fiber_trap_handler = NULL;
        if (neon_fiber_finished(f)) {
            f->state = NEON_FIBER_DONE;
            neon_fiber_free(f); // frees a crashed fiber's abandoned stack and its arena too
        } else {
            neon_sched_enqueue(f);
        }
    }

    t_sched.active = false;
}

void neon_fiber_spawn(neon_fiber_fn fn, void* arg) {
    if (!t_sched.active) {
        neon_trap("neon_fiber_spawn: no scheduler — call from inside fiber::runtime");
    }
    neon_sched_enqueue(neon_fiber_new(fn, arg, 0));
}

void neon_fiber_sched_yield(void) {
    // Hand control back to the pump loop, which re-enqueues us. neon_fiber_yield traps if
    // this is not a fiber, which is the right diagnostic for a stray call.
    neon_fiber_yield();
}
