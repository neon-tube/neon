#ifndef NEON_FIBER_INTERNAL_H
#define NEON_FIBER_INTERNAL_H

// Shared between the fiber primitive (src/fiber.c) and the scheduler (src/fiber_sched.c):
// the fiber struct and its scheduler-facing fields. Not part of the ABI — like internal.h it
// lives in src/ and is never installed; generated C sees only the opaque `neon_fiber` from
// neon/fiber.h. The two files are split because a raw stack-swap primitive and a run queue
// are genuinely different concerns, but they share one struct, and a src-only header is the
// clean way to let both reach it without widening the public surface.

#include "neon/arena.h"
#include "neon/fiber.h"

#include <stdbool.h>
#include <stddef.h>

// Scheduler state. Only src/fiber_sched.c reads or writes `state`; the primitive leaves it at
// the zero neon_fiber_new starts it in.
enum {
    NEON_FIBER_READY = 0, // on the run queue (or freshly created), waiting to run
    NEON_FIBER_RUNNING,   // currently executing
    NEON_FIBER_DONE,      // body has returned
};

struct neon_fiber {
    void* sp;                 // suspended stack pointer — valid exactly while not running
    void* stack;              // the heap stack allocation; NULL for the adopted root fiber
    const void* stack_bottom; // low address of the usable stack, for the ASan annotations
    size_t stack_size;
    void* fake_stack;         // ASan fake-stack save slot while this fiber is switched-away
    neon_arena* arena;        // this fiber's private heap; NULL for the root fiber
    neon_fiber_fn fn;
    void* arg;
    neon_fiber* link;         // resume-link: control returns here on yield or finish
    bool finished;
    bool is_root;             // the thread's original context, adopted rather than allocated
    neon_fiber* q_next;       // intrusive run-queue link, owned by src/fiber_sched.c
    int state;                // one of the NEON_FIBER_* above, owned by src/fiber_sched.c
};

#endif
