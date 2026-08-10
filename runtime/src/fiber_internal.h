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

#include "platform.h" // neon_ssize / neon_iovec, for the uring operation signatures

#include <stdatomic.h>
#include <stdbool.h>
#include <stddef.h>

// Scheduler state. Only src/fiber_sched.c reads or writes `state`; the primitive leaves it at
// the zero neon_fiber_new starts it in.
enum {
    NEON_FIBER_READY = 0, // on the run queue (or freshly created), waiting to run
    NEON_FIBER_RUNNING,   // currently executing
    NEON_FIBER_BLOCKED,   // parked off the run queue, waiting to be woken (a channel, a Task)
    NEON_FIBER_DONE,      // body has returned
};

struct neon_fiber {
    void* sp;                 // suspended stack pointer — valid exactly while not running
    void* stack;              // the stack allocation base (mmap or malloc); NULL for the root
    size_t map_len;           // mmap length for munmap (0 when the stack came from malloc)
    const void* stack_bottom; // low address of the USABLE stack (just above the guard page)
    size_t stack_size;
    void* fake_stack;         // ASan fake-stack save slot while this fiber is switched-away
    neon_arena* arena;        // this fiber's private heap; NULL for the root fiber
    neon_fiber_fn fn;
    void* arg;
    neon_fiber* link;         // resume-link: control returns here on yield or finish
    bool finished;
    bool crashed;             // finished via a trap (crash isolation), not a normal return
    bool is_root;             // the thread's original context, adopted rather than allocated
    neon_fiber* q_next;       // intrusive run-queue link, owned by src/fiber_sched.c
    int state;                // one of the NEON_FIBER_* above, owned by src/fiber_sched.c
    // Called by the scheduler when this fiber is reaped, with `crashed` saying how it went.
    // How a task learns its body died: crash propagation's one seam. NULL for plain fibers.
    void (*on_reap)(void* arg, bool crashed);
    void* on_reap_arg;
    // The running closure's environment (shared heap / slab — NOT in this fiber's arena),
    // so a CRASH can release it: the normal-exit release in neon_fiber_run_closure is on
    // the abandoned stack. Without this a crashed fiber leaked its env — and once a moved
    // resource's owning ref lives in that env, the leak would be an fd whose cleanup never
    // runs. Set before the body runs, cleared on the normal path; teardown releases it.
    neon_header* body_env;
    // Set (CAS) by every path that admits this fiber to a run queue — local enqueue,
    // remote injection — and cleared by the pump after dequeue. What makes a wake
    // IDEMPOTENT: a deadline firing and a channel delivery racing for the same parked
    // fiber both try to admit it, and exactly one wins; the loser's wake dissolves. A
    // stale wake aimed at a fiber that has since yielded merely re-runs it once.
    _Atomic int queued;
    // The scheduler this fiber is PINNED to (a neon_sched*, opaque here). Every wake routes
    // through it; only its thread ever resumes this fiber or touches its arena. NULL for
    // raw-primitive fibers driven outside a scheduler (the fiber_test.c suite).
    void* home;
};

// ---- the scheduler/offload seam (fiber_sched.c <-> fiber_offload.c) ----

// Register `fd` persistently in the scheduler's epoll with `tag` as its cookie; the pump
// recognises `&neon_offload_tag` and drains completions instead of waking a fiber.
void neon_sched_epoll_add_tag(int fd, void* tag);

// Count a fiber parked on out-of-scheduler work (an offloaded syscall) as an IO waiter, so
// an idle queue means "wait in the kernel", not deadlock.
void neon_sched_io_waiter_begin(void);
void neon_sched_io_waiter_end(void);

// ---- the io_uring backend (fiber_uring.c) ----
//
// A completion engine under the same seams as the epoll reactor: when a seat has a ring,
// the operation hooks submit the operation itself (no offload pool) and the pump idles in
// io_uring_enter. Absent a ring — old kernel, seccomp, or NEON_IO=epoll — every entry
// point declines and the epoll+offload path stands.

// One in-flight operation, living on the PARKED FIBER'S OWN STACK: it stays valid exactly
// as long as the fiber is parked, which is exactly as long as the completion can arrive.
// Its address is the SQE's user_data, so a completion names its own waiter.
typedef struct neon_uring_req {
    neon_fiber* fiber;
    int32_t res; // the kernel's result: >= 0 a count, < 0 a negated errno
    bool done;
} neon_uring_req;

bool neon_uring_open(void);   // false when io_uring is unavailable here
void neon_uring_close(void);
bool neon_uring_active(void); // whether THIS seat has a ring
neon_ssize neon_uring_read(int fd, void* buf, size_t n);
neon_ssize neon_uring_writev(int fd, const neon_iovec* iov, int n);
bool neon_uring_poll(int fd, uint32_t events); // false if it could not be submitted
void neon_uring_watch_doorbell(int fd);        // the cross-seat doorbell, as a ring poll
bool neon_sys_uring_here(void);                // std::sys::io_engine's probe
void neon_uring_wait(int timeout_ms);          // idle: block for a completion or the deadline
void neon_uring_poll_completions(void);        // non-blocking drain

extern char neon_offload_tag;
void neon_offload_drain(void);
void neon_offload_arm(void);
void neon_offload_disarm(void);

// Called via the trap→fiber bridge (neon_fiber_trap_handler) when the running fiber traps:
// mark it crashed and switch back to its resume-link, abandoning this stack. Reuses the
// ASan-annotated context swap the normal exit uses — no setjmp/longjmp. NORETURN.
void neon_fiber_on_trap(void);

#endif
