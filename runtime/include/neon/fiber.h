#ifndef NEON_FIBER_H
#define NEON_FIBER_H

// A stackful, cooperatively-scheduled fiber — the execution half of the design in
// `docs/design/fibers.md`. Suspension is a stack swap (`neon_ctx_swap`, ~15 instructions of
// per-ABI assembly), so fibers are uncolored: a fiber that blocks looks like an ordinary
// call, no `async`.
//
// This header is layered: the raw swap primitive first (a fiber owns a stack, an arena, and
// a suspended stack pointer; `neon_fiber_resume` runs one until it yields or finishes), then
// the scheduler, then the `std::fiber` native surface. Every switch is bracketed with
// AddressSanitizer's fiber annotations (`__sanitizer_start_switch_fiber` /
// `_finish_switch_fiber`), whose absence silently corrupts ASan's fake-stack bookkeeping
// across a raw stack swap.
//
// M:N by construction: "who is running now" is per-OS-thread, a switch names both sides
// explicitly, and fibers are PINNED to a home seat — so nothing here assumes one scheduler
// thread, and a fiber's arena is only ever touched by its own.

#include <stdbool.h>
#include <stddef.h>

#include "neon/core.h" // neon_closure, for the std::fiber native surface

typedef struct neon_fiber neon_fiber;

// The body a fiber runs. When it returns, the fiber is finished and control goes to the
// fiber that most recently resumed it (its resume-link). Results travel through channels or
// a `Task`, not a return value.
typedef void (*neon_fiber_fn)(void* arg);

// Create a suspended fiber with its own stack (`stack_size` rounded up to a floor and
// aligned). It runs `fn(arg)` when first resumed. Nothing executes until then.
neon_fiber* neon_fiber_new(neon_fiber_fn fn, void* arg, size_t stack_size);

// The handle for the context running on this thread, the original thread context included.
// The first call on a thread adopts that thread's own stack as a fiber so control can be
// resumed back into it. Never NULL.
neon_fiber* neon_fiber_current(void);

// Resume `target` from the current fiber, recording the current fiber as `target`'s
// resume-link. Returns when `target` yields back or finishes. `target` must not be finished
// and must not be the current fiber.
void neon_fiber_resume(neon_fiber* target);

// Suspend the current fiber and return control to its resume-link (whoever last resumed it).
// Returns when the fiber is next resumed. Must not be called on the root thread fiber, which
// has no resume-link.
void neon_fiber_yield(void);

// Whether `f` has run its body to completion. A finished fiber must not be resumed.
bool neon_fiber_finished(const neon_fiber* f);

// Free a finished (or never-resumed) fiber, its arena, and its stack. The arena is WALKED
// first — every still-live object's drop forced, so resource cleanups run and outgoing
// references release exactly once — and then bulk-freed.
void neon_fiber_free(neon_fiber* f);

// ---- the scheduler (src/fiber_sched.c) ----
//
// One cooperative run queue per scheduler seat, over the primitive above. `runtime` opens a
// single seat; `runtime_threads` opens N and pins fibers across them.

// Run `body(arg)` as the first fiber on a fresh scheduler, pumping its run queue until every
// fiber — the first and everything it transitively spawns — has finished, then tear the
// scheduler down. This is the lazy `fiber::runtime(...)` entry: nothing exists until it is
// called, and it returns when the whole fiber tree is done. Call it from a non-fiber (root)
// context; nesting it inside a running fiber traps.
void neon_fiber_runtime(neon_fiber_fn body, void* arg);

// As neon_fiber_runtime, with `threads` scheduler seats (this thread is one of them).
// Fibers are PINNED: spawn round-robins them across seats and every later wake routes to
// the home seat, so a fiber's arena is only ever touched by its own thread. threads <= 1
// is exactly neon_fiber_runtime.
void neon_fiber_runtime_threads(int64_t threads, neon_fiber_fn body, void* arg);

// From within a running fiber: create a fiber running `fn(arg)` and enqueue it to run later
// under the same scheduler. Traps if there is no scheduler (i.e. called off-fiber). Returns
// the created fiber — owned by the scheduler, valid until it is reaped; a caller may attach
// bookkeeping (a task's reap hook) but must not free it.
neon_fiber* neon_fiber_spawn(neon_fiber_fn fn, void* arg);

// From within a running fiber: yield cooperatively. The scheduler runs the other ready
// fibers, then this one again. The explicit form; codegen also emits an implicit safepoint
// at every loop back-edge, which the preemption timer drives.
void neon_fiber_sched_yield(void);

// Park the current fiber: it leaves the run queue until neon_fiber_wake re-admits it, and the
// scheduler runs everything else meanwhile. The caller MUST register a waker somewhere a
// runnable fiber can reach (a channel's waiter list, a Task) before parking; otherwise, once
// the run queue empties with this fiber still parked, the scheduler reports a deadlock rather
// than hanging. Returns when woken. The blocking primitive channels and Task/await are built
// on (src/fiber_chan.c).
void neon_fiber_park(void);

// Park the calling fiber until `millis` from now (a zero or negative sleep is a plain
// yield); the scheduler runs everyone else and waits in the kernel only when idle. What
// `time::sleep` becomes inside a fiber runtime.
void neon_fiber_sleep(int64_t millis);

// Park with a deadline: true when an external wake arrived first, false when the deadline
// fired (the loser of the race is cancelled either way). What a timed receive builds on.
bool neon_fiber_park_deadline(int64_t millis);

// Re-admit a parked fiber to the run queue. Call it from the fiber that satisfied whatever
// `f` was waiting on (a send, a Task completion). Single-thread M=1: no queue race.
void neon_fiber_wake(neon_fiber* f);

// ---- the `std::fiber` native surface (src/fiber_lang.c) ----
//
// What the stdlib's `@native` declarations bind to: a Neon `() -> ()` closure arrives as an
// owned 16-byte `neon_closure` by value, is called on the new fiber's stack, and its
// environment released after. A CAPTURING closure crosses too — an arena-resident
// environment is deep-copied to the shared heap through the env-copy table below.

// The env-copy table: per closure-environment shape, the shape's DROP function (the
// runtime's only handle on a closure's type at spawn time) mapped to a deep copy of its
// captures. Emitted by codegen into every fiber program; weak-defaulted empty in the
// runtime so the C test binary links. This is what lets a CAPTURING closure cross to a
// spawned fiber: the environment is deep-copied to the shared heap like any sent value.
typedef struct {
    void (*drop)(void*);
    neon_header* (*copy)(const neon_header* env);
} neon_env_copy_entry;
// Pointer + count rather than an extern array: the strong (program) and weak (runtime
// default) definitions must have IDENTICAL types, and an array's length is part of its
// type — mismatched bounds are exactly the kind of thing LTO refuses to interpose.
extern const neon_env_copy_entry* neon_env_copy_entries;
extern size_t neon_env_copy_count;

// Deep-copy an ARENA-resident closure environment to the shared heap via the table,
// releasing the original (in the caller's context) and returning the copy. Traps if the
// shape is not in the table — a compiler bug, not a user error.
neon_header* neon_env_copy_to_shared(neon_header* env);

// `fiber::runtime(body)` — run `body` as the first fiber, pump until the whole tree is done.
void neon_fiber_lang_runtime(neon_closure body);

// `fiber::runtime_threads(n, body)` — the M:N entry.
void neon_fiber_lang_runtime_threads(int64_t threads, neon_closure body);

// `fiber::spawn(body)` — enqueue `body` as a fiber under the running scheduler.
void neon_fiber_lang_spawn(neon_closure body);

// ---- safepoint preemption (src/fiber_sched.c) ----
//
// Cooperative scheduling starves on a fiber that never yields. Codegen emits a call to
// neon_fiber_safepoint at loop back-edges; it is a cheap flag check that yields only when
// preemption has been requested (by a timer in the production build). Safe to call anywhere —
// a no-op outside a fiber.
void neon_fiber_safepoint(void);

// Request that the current fiber yield at its next safepoint. The production trigger is a
// timer signal; exposed so the mechanism is drivable without one.
void neon_fiber_request_preempt(void);

// ---- descriptor waits: the completion-IO seam (Linux, src/fiber_sched.c) ----
//
// The design's production IO is a proactor (io_uring/IOCP, kqueue/epoll-adapted, plus a
// file-offload pool); what is wired here is the scheduler seam it plugs into, over epoll. A
// fiber that would block on a descriptor parks, the scheduler runs everyone else and waits in
// the kernel only when idle, and wakes the fiber on readiness.
#if defined(__linux__)
#include <stdint.h>
#include <sys/types.h> // ssize_t

// Park the current fiber until `fd` is ready for `events` (an epoll mask, e.g. EPOLLIN). One
// waiter per fd. Must be called from within a fiber.
void neon_fiber_io_wait(int fd, uint32_t events);

// As above, bounded by a CLOCK_MONOTONIC deadline: true if `fd` came ready, false if the
// deadline passed first. The loser of the fd/timer race is cancelled so no stale wake can
// chase a fiber that has moved on. std::net's timed reads and connects reach it through
// the scheduler's deadline readiness hook.
bool neon_fiber_io_wait_deadline(int fd, uint32_t events, int64_t deadline_ms);

// A fiber-yielding read: on EAGAIN it waits for `fd` (non-blocking) to become readable,
// running other fibers meanwhile, and retries — transparent blocking, only the fiber waits.
ssize_t neon_fiber_read(int fd, void* buf, size_t count);
#endif

#endif
