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

#include <signal.h> // sig_atomic_t for the safepoint flag
#include <stddef.h>

// A completion-shaped IO integration. The design's production engine is a proactor —
// io_uring on Linux, IOCP on Windows, kqueue/epoll adapted elsewhere, with a thread pool
// offloading the file operations those readiness interfaces cannot do async. What is built
// here is the SCHEDULER SEAM that engine plugs into, and a readiness reactor (epoll) proving
// it end to end: a fiber that would block on a file descriptor parks, the pump runs everyone
// else, and only when nothing is runnable does it wait in the kernel for a descriptor to be
// ready — then wakes the fibers waiting on it. Swapping epoll for io_uring changes this file
// and nothing above it. Linux only for now; the fiber sources are already x86-64/Unix-gated.
#if defined(__linux__)
#  define NEON_FIBER_IO 1
#  include <errno.h>
#  include <sys/epoll.h>
#  include <unistd.h>
#endif

// One scheduler per OS thread. `active` guards against nesting a runtime inside a fiber and
// tells spawn there is a queue to add to. `live` counts fibers that have been spawned but not
// finished — including parked ones off the run queue — so that an empty run queue with live
// fibers left is recognised as deadlock (everything blocked, nothing to wake it) rather than
// a normal drain. `io_waiters` is how many of those live fibers are parked on a descriptor
// (not on each other): while any are, an empty run queue means "wait in the kernel", not
// deadlock.
typedef struct {
    neon_fiber* head;
    neon_fiber* tail;
    int live;
    int io_waiters;
    bool active;
#if defined(NEON_FIBER_IO)
    int epfd; // epoll instance, created lazily on the first IO wait; -1 until then
#endif
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

#if defined(NEON_FIBER_IO)
// Block in the kernel until at least one awaited descriptor is ready, then wake every fiber
// waiting on a ready one. Called only when nothing is runnable, so waiting here cannot starve
// a ready fiber. Registrations are EPOLLONESHOT with the waiting fiber in `data.ptr`.
static void neon_sched_poll_io(void) {
    struct epoll_event evs[16];
    int n;
    do {
        n = epoll_wait(t_sched.epfd, evs, 16, -1);
    } while (n < 0 && errno == EINTR);
    if (n < 0) {
        neon_trap("epoll_wait failed");
    }
    for (int i = 0; i < n; i++) {
        neon_fiber_wake((neon_fiber*)evs[i].data.ptr); // re-admit; the fiber finishes its wait
    }
}
#endif

void neon_fiber_runtime(neon_fiber_fn body, void* arg) {
    if (t_sched.active) {
        neon_trap("neon_fiber_runtime: a scheduler is already running on this thread");
    }
    t_sched.active = true;
    t_sched.live = 0;
    t_sched.io_waiters = 0;
#if defined(NEON_FIBER_IO)
    t_sched.epfd = -1; // created lazily on the first IO wait
#endif

    neon_sched_enqueue(neon_fiber_new(body, arg, 0));
    t_sched.live++;

    // The pump. A finished fiber is freed (arena bulk-dropped) and drops the live count; a
    // yielded one is re-enqueued; a PARKED one (a channel, a Task, or a descriptor) is left
    // off the queue, still live, for its waker. When nothing is runnable but fibers are parked
    // on descriptors, wait in the kernel for one to be ready rather than call it a deadlock.
    for (;;) {
        neon_fiber* f = neon_sched_dequeue();
        if (f == NULL) {
#if defined(NEON_FIBER_IO)
            if (t_sched.io_waiters > 0) {
                neon_sched_poll_io();
                continue;
            }
#endif
            break;
        }
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
            t_sched.live--;
        } else if (f->state == NEON_FIBER_BLOCKED) {
            // parked: not re-enqueued, and still counted live until it is woken and finishes
        } else {
            neon_sched_enqueue(f);
        }
    }

    // The queue emptied with fibers still live AND none parked on a descriptor: every one is
    // waiting on another that will never run. That is a deadlock, and silence would be a hang.
    if (t_sched.live > 0) {
        neon_trap("deadlock: all fibers are blocked");
    }

#if defined(NEON_FIBER_IO)
    if (t_sched.epfd >= 0) {
        close(t_sched.epfd);
        t_sched.epfd = -1;
    }
#endif
    t_sched.active = false;
}

void neon_fiber_spawn(neon_fiber_fn fn, void* arg) {
    if (!t_sched.active) {
        neon_trap("neon_fiber_spawn: no scheduler — call from inside fiber::runtime");
    }
    neon_sched_enqueue(neon_fiber_new(fn, arg, 0));
    t_sched.live++;
}

void neon_fiber_sched_yield(void) {
    // Hand control back to the pump loop, which re-enqueues us. neon_fiber_yield traps if
    // this is not a fiber, which is the right diagnostic for a stray call.
    neon_fiber_yield();
}

void neon_fiber_park(void) {
    neon_fiber* cur = neon_fiber_current();
    if (cur->is_root) {
        neon_trap("neon_fiber_park: the root context cannot park");
    }
    // Mark blocked and hand control back: the pump loop sees BLOCKED and leaves us off the
    // queue. We resume here only when neon_fiber_wake re-admits us. The caller must have
    // handed a waker somewhere reachable (a channel's waiter list, a Task) before parking, or
    // the scheduler will detect the deadlock rather than hang.
    cur->state = NEON_FIBER_BLOCKED;
    neon_fiber_yield();
}

void neon_fiber_wake(neon_fiber* f) {
    // Re-admit a parked fiber. Enqueue resets its state to READY. Safe to call from within
    // another fiber (the common case: a sender waking a blocked receiver) — it runs on the
    // same thread under the same scheduler, so there is no queue race in the M=1 build.
    neon_sched_enqueue(f);
}

// ---- descriptor waits (the IO seam) ----

#if defined(NEON_FIBER_IO)
// Park the current fiber until `fd` is ready for `events` (an epoll mask, e.g. EPOLLIN). The
// pump waits in the kernel on our behalf when nothing else can run, and wakes us on
// readiness. One waiter per fd (the registration is exclusive); the fd is removed on return.
void neon_fiber_io_wait(int fd, uint32_t events) {
    neon_fiber* cur = neon_fiber_current();
    if (cur->is_root) {
        neon_trap("neon_fiber_io_wait: the root context cannot wait on IO");
    }
    if (t_sched.epfd < 0) {
        t_sched.epfd = epoll_create1(EPOLL_CLOEXEC);
        if (t_sched.epfd < 0) {
            neon_trap("epoll_create1 failed");
        }
    }
    struct epoll_event ev;
    ev.events = events | EPOLLONESHOT;
    ev.data.ptr = cur;
    if (epoll_ctl(t_sched.epfd, EPOLL_CTL_ADD, fd, &ev) != 0) {
        neon_trap("epoll_ctl(ADD) failed");
    }
    t_sched.io_waiters++;
    neon_fiber_park(); // the pump's epoll_wait wakes us when fd is ready
    t_sched.io_waiters--;
    epoll_ctl(t_sched.epfd, EPOLL_CTL_DEL, fd, NULL); // ONESHOT already disarmed it; tidy up
}

// A read that yields the fiber instead of blocking the thread: on EAGAIN it waits for the
// descriptor to become readable (running other fibers meanwhile) and retries. `fd` must be
// non-blocking. This is the shape every IO call takes — the transparent blocking the design
// promises: it reads like an ordinary blocking read, but only the fiber waits.
ssize_t neon_fiber_read(int fd, void* buf, size_t count) {
    for (;;) {
        ssize_t r = read(fd, buf, count);
        if (r >= 0) {
            return r;
        }
        if (errno == EINTR) {
            continue;
        }
        if (errno == EAGAIN || errno == EWOULDBLOCK) {
            neon_fiber_io_wait(fd, EPOLLIN);
            continue;
        }
        return -1;
    }
}
#endif

// ---- safepoint preemption ----
//
// Cooperative scheduling starves on a fiber that never yields. The design's answer is
// compiler-inserted safepoints at loop back-edges: cheap checks of a per-thread flag that
// yield when it is set. A timer (SIGALRM/SIGVTALRM in the production build) sets the flag, so
// a hot loop is preempted at its next back-edge without the programmer writing a yield. Here
// is the runtime half — the flag, the check, and a way to request preemption; the timer
// trigger and the compiler's safepoint emission are the halves above this file.
static _Thread_local volatile sig_atomic_t t_preempt;

// Emitted by codegen at loop back-edges. If preemption was requested, clear it and yield;
// otherwise a handful of instructions and on. A no-op outside a fiber (the root has nowhere to
// yield), so it is safe to call unconditionally.
void neon_fiber_safepoint(void) {
    if (!t_preempt) {
        return;
    }
    t_preempt = 0;
    neon_fiber* cur = neon_fiber_current();
    if (!cur->is_root) {
        neon_fiber_sched_yield();
    }
}

// Request that the current fiber yield at its next safepoint. The production trigger is a
// timer signal; exposed here so the mechanism is drivable and testable without one.
void neon_fiber_request_preempt(void) {
    t_preempt = 1;
}
