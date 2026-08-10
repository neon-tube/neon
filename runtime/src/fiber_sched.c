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

#include "platform.h" // neon_plat_sleep_ms, for the root-context sleep fallback

#include <pthread.h>   // the mesh: per-scheduler lock, worker threads
#include <signal.h>    // sig_atomic_t for the safepoint flag, the preemption timer's handler
#include <stdatomic.h> // the mesh's global live/ext/waiting accounting
#include <time.h>      // clock_gettime for sleep deadlines
#include <stddef.h>
#include <string.h> // memset for the timer's sigaction/itimerval setup

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
#  include <sys/eventfd.h>
#  include <unistd.h>
#endif

// One scheduler per OS thread. `active` guards against nesting a runtime inside a fiber and
// tells spawn there is a queue to add to. `live` counts fibers that have been spawned but not
// finished — including parked ones off the run queue — so that an empty run queue with live
// fibers left is recognised as deadlock (everything blocked, nothing to wake it) rather than
// a normal drain. `io_waiters` is how many of those live fibers are parked on a descriptor
// (not on each other): while any are, an empty run queue means "wait in the kernel", not
// deadlock.
// A sleeping fiber: parked until its deadline. On the sleeper's own stack, like a channel
// waiter — parking costs no heap.
typedef struct neon_sleeper {
    neon_fiber* fiber;
    int64_t deadline_ms; // CLOCK_MONOTONIC milliseconds
    bool expired;        // set when the DEADLINE woke this fiber (vs an external wake)
    struct neon_sleeper* next;
} neon_sleeper;

typedef struct neon_sched {
    neon_fiber* head; // the LOCAL run queue: owner-thread only, lock-free
    neon_fiber* tail;
    int live;       // M=1 accounting (kept for the single-scheduler entry)
    int io_waiters;
    bool active;
    neon_sleeper* sleepers; // under `mu` when the mesh is up: remote wakes unlink here
#if defined(NEON_FIBER_IO)
    int epfd; // epoll instance, created lazily on the first IO wait; -1 until then
#endif
    // ---- the mesh (fibers are PINNED to a home scheduler; these are how the others
    // reach it) ----
    pthread_mutex_t mu;      // guards inj + sleepers; never held across a switch
    neon_fiber* inj_head;    // remote wakes and remote spawns land here...
    neon_fiber* inj_tail;
    int doorbell;            // ...and this eventfd (registered in epfd) interrupts an idle wait
    int id;
} neon_sched;

static _Thread_local neon_sched t_sched;

// The scheduler registry, live for the duration of one runtime call. Fibers are pinned:
// spawn round-robins them across schedulers, and every later wake routes to the home.
#define NEON_MAX_SCHEDS 64
static neon_sched* g_scheds[NEON_MAX_SCHEDS];
static int g_nscheds;               // written before workers run, read-only after
static _Atomic int g_spawn_ctr;     // round-robin placement
static _Atomic long g_live;         // fibers spawned minus reaped, across the mesh
static _Atomic long g_ext;          // fibers waiting on the WORLD (sleep, fd, offload)
static _Atomic int g_waiting;       // schedulers blocked in an indefinite wait
static _Atomic int g_done;          // set when g_live hits zero: every pump exits

// The preemption timer (defined with the safepoint machinery at the end of this file),
// armed for exactly the life of the runtime.
static void neon_fiber_timer_arm(void);
static void neon_fiber_timer_disarm(void);
// Remote-arrival drain (defined with wake, later in the file).
static void neon_sched_drain_inj(void);

static void neon_sched_enqueue(neon_fiber* f) {
    int zero = 0;
    if (!atomic_compare_exchange_strong(&f->queued, &zero, 1)) {
        return; // already admitted by a racing wake; exactly one admission stands
    }
    f->q_next = NULL;
    f->state = NEON_FIBER_READY;
    if (t_sched.tail != NULL) {
        t_sched.tail->q_next = f;
    } else {
        t_sched.head = f;
    }
    t_sched.tail = f;
}

// Admission that already HOLDS the queued bit (a drained injection entry): push raw.
static void neon_sched_enqueue_owned(neon_fiber* f) {
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
        atomic_store(&f->queued, 0); // off the queue: wakes may admit it again
    }
    return f;
}

static int64_t neon_sched_now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

// Wake every sleeper whose deadline has passed; return the milliseconds until the nearest
// remaining one, or -1 when nobody sleeps (epoll's "wait forever").
static int neon_sched_wake_due(void) {
    int64_t now = neon_sched_now_ms();
    int64_t nearest = -1;
    pthread_mutex_lock(&t_sched.mu); // remote wakes unlink sleepers concurrently
    neon_sleeper** link = &t_sched.sleepers;
    while (*link != NULL) {
        neon_sleeper* s = *link;
        if (s->deadline_ms <= now) {
            *link = s->next;
            s->expired = true;
            atomic_fetch_sub(&g_ext, 1);
            // Enqueue directly rather than via neon_fiber_wake, whose sleeper-cancel scan
            // would re-take this lock; the local queue is owner-only, so this is safe.
            neon_sched_enqueue(s->fiber);
            continue;
        }
        int64_t left = s->deadline_ms - now;
        if (nearest < 0 || left < nearest) {
            nearest = left;
        }
        link = &s->next;
    }
    pthread_mutex_unlock(&t_sched.mu);
    if (nearest > 2147483647) {
        nearest = 2147483647; // epoll takes an int; a 24-day sleep can re-wait
    }
    return (int)nearest;
}

// Park the calling fiber until `millis` from now. The sleeper record lives on this frame,
// which persists while parked; the pump wakes it when the deadline passes.
void neon_fiber_sleep(int64_t millis) {
    if (millis <= 0) {
        neon_fiber_sched_yield(); // a zero sleep is still a scheduling point
        return;
    }
    // +1: deadlines are whole milliseconds and `now` truncates, so without the round-up a
    // sleep could come back a fraction of a millisecond EARLY — `sleep` promises at least.
    neon_sleeper s = {neon_fiber_current(), neon_sched_now_ms() + millis + 1, false, NULL};
    pthread_mutex_lock(&t_sched.mu);
    s.next = t_sched.sleepers;
    t_sched.sleepers = &s;
    pthread_mutex_unlock(&t_sched.mu);
    atomic_fetch_add(&g_ext, 1);
    neon_fiber_park();
    if (!s.expired) {
        atomic_fetch_sub(&g_ext, 1); // woken externally: wake_due did not account for us
    }
}

// Park with a deadline: returns true when an external neon_fiber_wake arrived first, false
// when the deadline fired. The one primitive a timed receive needs — the waker and the
// timer race, and whichever loses is CANCELLED (the wake pulls the sleeper off the list;
// the caller unlinks its own waiter on a timeout), so no stale wake can reach a fiber that
// has moved on.
bool neon_fiber_park_deadline(int64_t millis) {
    if (millis <= 0) {
        return false; // an expired deadline before parking: the timeout outcome
    }
    neon_sleeper s = {neon_fiber_current(), neon_sched_now_ms() + millis + 1, false, NULL};
    pthread_mutex_lock(&t_sched.mu);
    s.next = t_sched.sleepers;
    t_sched.sleepers = &s;
    pthread_mutex_unlock(&t_sched.mu);
    atomic_fetch_add(&g_ext, 1);
    neon_fiber_park();
    if (!s.expired) {
        // An external wake won the race; neon_fiber_wake already unlinked the sleeper.
        atomic_fetch_sub(&g_ext, 1);
        return true;
    }
    return false;
}

#if defined(NEON_FIBER_IO)
#include <sys/syscall.h>
// Park until `pid` exits, via pidfd + epoll; the caller's waitpid then returns at once. On
// the root context, or if pidfd_open is unavailable, do nothing — the caller's plain
// blocking waitpid stands.
static void neon_sched_pidwait_hook(int64_t pid) {
    if (neon_fiber_current()->is_root) {
        return;
    }
    int pidfd = (int)syscall(SYS_pidfd_open, (pid_t)pid, 0);
    if (pidfd < 0) {
        return; // old kernel: fall back to the blocking wait
    }
    neon_fiber_io_wait(pidfd, EPOLLIN); // readable exactly when the process has exited
    close(pidfd);
}
#else
static void neon_sched_pidwait_hook(int64_t pid) { (void)pid; }
#endif

// The time::sleep hook (internal.h): fiber context parks, the root context — a resource
// cleanup running on the scheduler, say — keeps the plain blocking sleep.
static void neon_sched_sleep_hook(int64_t millis) {
    if (neon_fiber_current()->is_root) {
        neon_plat_sleep_ms(millis);
        return;
    }
    neon_fiber_sleep(millis);
}

// The operation hooks a seat installs when it has a ring: submit the operation and park.
// Off-fiber (a resource cleanup running on the root context) there is no fiber to park, so
// fall through to the plain blocking syscall — the same rule the offload hooks follow.
static neon_ssize neon_sched_uring_read_hook(int fd, void* buf, size_t n) {
    if (neon_fiber_current()->is_root) {
        return neon_plat_read(fd, buf, n);
    }
    return neon_uring_read(fd, buf, n);
}

static neon_ssize neon_sched_uring_writev_hook(int fd, const neon_iovec* iov, int n) {
    if (neon_fiber_current()->is_root) {
        return neon_plat_writev(fd, (neon_iovec*)iov, n);
    }
    return neon_uring_writev(fd, iov, n);
}

static void neon_fiber_uring_arm(void) {
    neon_fiber_blocking_read = neon_sched_uring_read_hook;
    neon_fiber_blocking_writev = neon_sched_uring_writev_hook;
}

#if defined(NEON_FIBER_IO)
static void neon_sched_ensure_epfd(void) {
    if (t_sched.epfd < 0) {
        t_sched.epfd = epoll_create1(EPOLL_CLOEXEC);
        if (t_sched.epfd < 0) {
            neon_trap("epoll_create1 failed");
        }
    }
}

// The offload pool's doorbell: a persistent registration whose cookie the pump recognises.
void neon_sched_epoll_add_tag(int fd, void* tag) {
    neon_sched_ensure_epfd();
    struct epoll_event ev;
    ev.events = EPOLLIN;
    ev.data.ptr = tag;
    if (epoll_ctl(t_sched.epfd, EPOLL_CTL_ADD, fd, &ev) != 0) {
        neon_trap("epoll_ctl(ADD, offload doorbell) failed");
    }
}

void neon_sched_io_waiter_begin(void) {
    t_sched.io_waiters++;
    atomic_fetch_add(&g_ext, 1);
}
void neon_sched_io_waiter_end(void) {
    t_sched.io_waiters--;
    atomic_fetch_sub(&g_ext, 1);
}

// Block in the kernel until at least one awaited descriptor is ready, then wake every fiber
// waiting on a ready one. Called only when nothing is runnable, so waiting here cannot starve
// a ready fiber. Registrations are EPOLLONESHOT with the waiting fiber in `data.ptr`.
static void neon_sched_poll_io(int timeout_ms) {
    struct epoll_event evs[16];
    int n;
    do {
        n = epoll_wait(t_sched.epfd, evs, 16, timeout_ms);
    } while (n < 0 && errno == EINTR);
    if (n < 0) {
        neon_trap("epoll_wait failed");
    }
    for (int i = 0; i < n; i++) {
        if (evs[i].data.ptr == &neon_offload_tag) {
            neon_offload_drain(); // completed offloaded syscalls: wake their fibers
            continue;
        }
        if (evs[i].data.ptr == &t_sched) {
            // Our doorbell: a remote scheduler pushed an injection or rang shutdown. Clear
            // it; the pump's next lap drains the queue and re-checks g_done.
            uint64_t v;
            ssize_t r = read(t_sched.doorbell, &v, sizeof(v));
            (void)r;
            continue;
        }
        neon_fiber_wake((neon_fiber*)evs[i].data.ptr); // re-admit; the fiber finishes its wait
    }
}
#endif

// Open this thread's scheduler: state, hooks, and — when the mesh has more than one seat —
// an eager epoll + doorbell so remote schedulers can reach it. Registration into the global
// seat table is the caller's line to cross (it knows the seat index).
static void neon_sched_open(bool mesh) {
    t_sched.active = true;
    t_sched.live = 0;
    t_sched.io_waiters = 0;
    t_sched.sleepers = NULL;
    t_sched.inj_head = NULL;
    t_sched.inj_tail = NULL;
    t_sched.doorbell = -1;
    pthread_mutex_init(&t_sched.mu, NULL);
#if defined(NEON_FIBER_IO)
    t_sched.epfd = -1; // created lazily on the first IO wait...
    if (mesh) {
        neon_sched_ensure_epfd(); // ...but the mesh needs the doorbell listening NOW
        t_sched.doorbell = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
        if (t_sched.doorbell < 0) {
            neon_trap("eventfd failed");
        }
        neon_sched_epoll_add_tag(t_sched.doorbell, &t_sched);
    }
#else
    (void)mesh;
#endif
    // This seat's io_uring, when the kernel has one. With a ring, the operation hooks
    // submit the operation ITSELF and the offload pool is never armed — files need no
    // worker threads under a completion engine. Without one (old kernel, seccomp,
    // NEON_IO=epoll) the pool arms and the epoll reactor stands, unchanged.
    if (neon_uring_open()) {
        neon_fiber_uring_arm();
        // The doorbell must be watched by the engine this seat actually IDLES in: a seat
        // blocked in io_uring_enter cannot be reached through an epoll set. Registering it
        // on the ring is what lets a remote wake interrupt an idle seat.
        if (mesh && t_sched.doorbell >= 0) {
            neon_uring_watch_doorbell(t_sched.doorbell);
        }
    } else {
        neon_offload_arm(); // file reads/writes park fibers via the pool from here on
    }
    neon_fiber_sleep_hook = neon_sched_sleep_hook; // time::sleep parks fibers from here on
    neon_fiber_pidwait_hook = neon_sched_pidwait_hook; // process waits park via pidfd
    neon_fiber_timer_arm(); // THIS seat's own preemption timer
}

static void neon_sched_close(void) {
    neon_fiber_timer_disarm(); // this seat's timer
    neon_fiber_sleep_hook = NULL;
    neon_offload_disarm();
    neon_uring_close();
    neon_fiber_pidwait_hook = NULL;
#if defined(NEON_FIBER_IO)
    if (t_sched.doorbell >= 0) {
        close(t_sched.doorbell);
        t_sched.doorbell = -1;
    }
    if (t_sched.epfd >= 0) {
        close(t_sched.epfd);
        t_sched.epfd = -1;
    }
#endif
    pthread_mutex_destroy(&t_sched.mu);
    t_sched.active = false;
}

// Ring every seat's doorbell — how "the last fiber finished" reaches schedulers blocked in
// an indefinite wait.
static void neon_sched_ring_all(void) {
    for (int i = 0; i < g_nscheds; i++) {
        if (g_scheds[i] != NULL && g_scheds[i]->doorbell >= 0) {
            uint64_t one = 1;
            ssize_t w = write(g_scheds[i]->doorbell, &one, sizeof(one));
            (void)w;
        }
    }
}

// The pump: one per scheduler thread, identical whether the mesh has one seat or many. A
// finished fiber is reaped (on-reap hook, teardown walk, free) and decrements the GLOBAL
// live count — whoever reaches zero ends the whole runtime. A parked fiber is left off the
// queue for its waker; remote arrivals drain from the injection queue each lap. When
// nothing is runnable: due sleepers wake, and whatever remains waits in the kernel — the
// nearest deadline, a ready descriptor, the offload pool's doorbell, or another
// scheduler's ring. A deadlock — every seat idle forever, nothing external pending, fibers
// still live — is trapped by the LAST seat to go idle, never hung.
static void neon_sched_pump(void) {
    for (;;) {
        neon_sched_drain_inj();
        if (neon_uring_active()) {
            neon_uring_poll_completions(); // non-blocking reap: publish + take what landed
        }
        neon_fiber* f = neon_sched_dequeue();
        if (f == NULL) {
            if (atomic_load(&g_done)) {
                break;
            }
            int timeout = neon_sched_wake_due();
            if (t_sched.head != NULL) {
                continue; // a sleeper came due and is runnable again
            }
#if defined(NEON_FIBER_IO)
            bool can_wait = t_sched.io_waiters > 0 || timeout >= 0 || t_sched.doorbell >= 0;
#else
            bool can_wait = false;
#endif
            if (!can_wait) {
                // One seat, nothing external, fibers still live: the classic deadlock.
                if (atomic_load(&g_live) > 0) {
                    neon_trap("deadlock: all fibers are blocked");
                }
                atomic_store(&g_done, 1);
                break;
            }
#if defined(NEON_FIBER_IO)
            if (timeout < 0) {
                // Indefinite wait: if every seat is now waiting indefinitely with nothing
                // external in flight and fibers still live, no wake can ever arrive. The
                // last seat in says so. (A waker is always a RUNNING fiber or an external
                // completion; the former means some seat is not here, the latter means
                // g_ext > 0 — so the check cannot fire while a wake is in flight.)
                int waiting = atomic_fetch_add(&g_waiting, 1) + 1;
                if (waiting == (g_nscheds > 0 ? g_nscheds : 1) && atomic_load(&g_ext) == 0 &&
                    atomic_load(&g_live) > 0 && !atomic_load(&g_done)) {
                    // Every seat is idle with nothing external outstanding — but "idle" was
                    // sampled, and a wake already pushed onto some seat's injection queue is
                    // work that no counter reflects until that seat looks. So RE-CHECK the
                    // queues before declaring deadlock: a real one has no queued fiber
                    // anywhere and will still be true a moment later, while a race resolves
                    // the instant the pending injection is seen. (Sampling alone was a
                    // 1-in-40 false trap on the mesh pipeline — found when the epoll path's
                    // different timing exposed it.)
                    bool any_queued = false;
                    for (int i = 0; i < g_nscheds && !any_queued; i++) {
                        neon_sched* s = g_scheds[i];
                        if (s == NULL) {
                            continue;
                        }
                        pthread_mutex_lock(&s->mu);
                        any_queued = s->inj_head != NULL || s->sleepers != NULL;
                        pthread_mutex_unlock(&s->mu);
                        if (!any_queued && s == &t_sched) {
                            any_queued = t_sched.head != NULL; // our own local queue
                        }
                    }
                    if (!any_queued && atomic_load(&g_ext) == 0 && atomic_load(&g_live) > 0 &&
                        !atomic_load(&g_done)) {
                        neon_trap("deadlock: all fibers are blocked");
                    }
                }
                // The idle wait, on whichever engine this seat opened. With a ring one call
                // carries the wait AND the deadline (a TIMEOUT SQE) and reaps what landed;
                // the epoll path needs its descriptors registered first. A seat with a ring
                // may still hold epoll registrations — the doorbell, and any POLL_ADD that
                // could not be submitted — so when both exist the ring waits briefly and
                // the epoll drain follows, rather than either blocking the other out.
                if (neon_uring_active()) {
                    if (t_sched.epfd >= 0) {
                        neon_uring_wait(1);
                        neon_sched_poll_io(0);
                    } else {
                        neon_uring_wait(-1);
                    }
                } else {
                    neon_sched_ensure_epfd();
                    neon_sched_poll_io(-1);
                }
                atomic_fetch_sub(&g_waiting, 1);
            } else if (neon_uring_active()) {
                if (t_sched.epfd >= 0) {
                    neon_uring_wait(timeout < 1 ? 1 : timeout);
                    neon_sched_poll_io(0);
                } else {
                    neon_uring_wait(timeout);
                }
            } else {
                neon_sched_ensure_epfd();
                neon_sched_poll_io(timeout);
            }
            continue;
#endif
        }
        f->state = NEON_FIBER_RUNNING;
        // Arm the trap→fiber bridge while a fiber runs, and disarm the moment we are back
        // on the root context so that a trap in the scheduler's own code stays fatal. A
        // crashed fiber comes back through the same resume return, flagged.
        neon_fiber_trap_handler = neon_fiber_on_trap;
        neon_fiber_resume(f);
        neon_fiber_trap_handler = NULL;
        if (neon_fiber_finished(f)) {
            f->state = NEON_FIBER_DONE;
            if (f->on_reap != NULL) {
                f->on_reap(f->on_reap_arg, f->crashed);
            }
            neon_fiber_free(f);
            if (atomic_fetch_sub(&g_live, 1) == 1) {
                atomic_store(&g_done, 1); // the whole tree finished
                neon_sched_ring_all();
            }
        } else if (f->state == NEON_FIBER_BLOCKED) {
            // parked: off the queue, still live, waiting for its waker
        } else {
            neon_sched_enqueue(f);
        }
    }
}

void neon_fiber_runtime(neon_fiber_fn body, void* arg) {
    if (t_sched.active) {
        neon_trap("neon_fiber_runtime: a scheduler is already running on this thread");
    }
    atomic_store(&g_live, 0);
    atomic_store(&g_ext, 0);
    atomic_store(&g_waiting, 0);
    atomic_store(&g_done, 0);
    atomic_store(&g_spawn_ctr, 0);
    g_nscheds = 1;
    g_scheds[0] = &t_sched;
    t_sched.id = 0;
    neon_sched_open(false);

    neon_fiber* first = neon_fiber_new(body, arg, 0);
    first->home = &t_sched;
    atomic_fetch_add(&g_live, 1);
    neon_sched_enqueue(first);

    neon_sched_pump();

    neon_sched_close();
    g_scheds[0] = NULL;
    g_nscheds = 0;
}

// A worker seat: open, announce, pump until the runtime ends, close. The seat index rides
// in as the argument; announcement is the store into the seat table, which the launcher
// spins on before the first fiber runs — so placement is stable by the time it matters.
typedef struct {
    int id;
    _Atomic int* ready;
} neon_sched_seat;

static void* neon_sched_worker_main(void* argp) {
    neon_sched_seat* seat = (neon_sched_seat*)argp;
    t_sched.id = seat->id;
    neon_sched_open(true);
    g_scheds[seat->id] = &t_sched;
    atomic_fetch_add(seat->ready, 1);
    neon_sched_pump();
    g_scheds[t_sched.id] = NULL;
    neon_sched_close();
    return NULL;
}

// The M:N entry: `threads` schedulers (this thread is seat 0), fibers pinned by spawn's
// round-robin, the whole mesh draining until the fiber tree is done.
void neon_fiber_runtime_threads(int64_t threads, neon_fiber_fn body, void* arg) {
    if (threads <= 1) {
        neon_fiber_runtime(body, arg);
        return;
    }
    if (t_sched.active) {
        neon_trap("neon_fiber_runtime: a scheduler is already running on this thread");
    }
    if (threads > NEON_MAX_SCHEDS) {
        threads = NEON_MAX_SCHEDS;
    }
    atomic_store(&g_live, 0);
    atomic_store(&g_ext, 0);
    atomic_store(&g_waiting, 0);
    atomic_store(&g_done, 0);
    atomic_store(&g_spawn_ctr, 0);
    g_nscheds = (int)threads;
    t_sched.id = 0;
    neon_sched_open(true);
    g_scheds[0] = &t_sched;

    _Atomic int ready = 0;
    pthread_t tids[NEON_MAX_SCHEDS];
    neon_sched_seat seats[NEON_MAX_SCHEDS];
    for (int i = 1; i < (int)threads; i++) {
        seats[i].id = i;
        seats[i].ready = &ready;
        if (pthread_create(&tids[i], NULL, neon_sched_worker_main, &seats[i]) != 0) {
            neon_trap("scheduler worker thread failed to start");
        }
    }
    while (atomic_load(&ready) != (int)threads - 1) {
        // spin briefly: workers only initialize state here, no fiber has run yet
    }

    neon_fiber* first = neon_fiber_new(body, arg, 0);
    first->home = &t_sched;
    atomic_fetch_add(&g_live, 1);
    neon_sched_enqueue(first);

    neon_sched_pump();

    for (int i = 1; i < (int)threads; i++) {
        pthread_join(tids[i], NULL);
    }
    neon_sched_close();
    g_scheds[0] = NULL;
    g_nscheds = 0;
}

neon_fiber* neon_fiber_spawn(neon_fiber_fn fn, void* arg) {
    if (!t_sched.active) {
        neon_trap("neon_fiber_spawn: no scheduler — call from inside fiber::runtime");
    }
    neon_fiber* f = neon_fiber_new(fn, arg, 0);
    atomic_fetch_add(&g_live, 1);
    // Placement: round-robin over the mesh. With one scheduler this is always "here"; with
    // more, a remote target gets the fiber through its injection queue — same road a
    // remote wake takes.
    int n = g_nscheds > 0 ? g_nscheds : 1;
    neon_sched* target = n > 1 ? g_scheds[(unsigned)atomic_fetch_add(&g_spawn_ctr, 1) % n]
                               : &t_sched;
    f->home = target;
    if (target == &t_sched) {
        neon_sched_enqueue(f);
    } else {
        pthread_mutex_lock(&target->mu);
        atomic_store(&f->queued, 1); // fresh fiber: uncontested claim
        f->q_next = NULL;
        if (target->inj_tail != NULL) {
            target->inj_tail->q_next = f;
        } else {
            target->inj_head = f;
        }
        target->inj_tail = f;
        pthread_mutex_unlock(&target->mu);
        uint64_t one = 1;
        ssize_t w = write(target->doorbell, &one, sizeof(one));
        (void)w;
    }
    return f;
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
    // Route by HOME: fibers are pinned, and only the home's thread touches its local run
    // queue. A local wake (the waker runs on the same scheduler) is the M=1 fast path; a
    // remote wake pushes onto the home's locked injection queue and rings its doorbell —
    // the home drains injections every pump iteration and the doorbell interrupts an idle
    // wait. The remote push deliberately does NOT touch f->state: the fiber may still be
    // mid-park on its home thread, and only the home (after the switch completes) may
    // write it — pinning is what makes the park/wake race unlosable.
    //
    // Either way, cancel a pending sleeper first: a deadline firing later for a fiber that
    // was woken (and may since have finished, or parked on something else entirely) would
    // be a wake aimed at whatever that fiber is now — the stale-wake bug every timed-wait
    // design has to close.
    neon_sched* home = (neon_sched*)f->home;
    if (home == NULL || home == &t_sched) {
        pthread_mutex_lock(&t_sched.mu);
        neon_sleeper** link = &t_sched.sleepers;
        while (*link != NULL) {
            if ((*link)->fiber == f) {
                *link = (*link)->next;
                break;
            }
            link = &(*link)->next;
        }
        pthread_mutex_unlock(&t_sched.mu);
        neon_sched_enqueue(f);
        return;
    }
    pthread_mutex_lock(&home->mu);
    neon_sleeper** link = &home->sleepers;
    while (*link != NULL) {
        if ((*link)->fiber == f) {
            *link = (*link)->next;
            break;
        }
        link = &(*link)->next;
    }
    int zero = 0;
    if (!atomic_compare_exchange_strong(&f->queued, &zero, 1)) {
        pthread_mutex_unlock(&home->mu);
        return; // a racing admission won; this wake dissolves
    }
    f->q_next = NULL;
    if (home->inj_tail != NULL) {
        home->inj_tail->q_next = f;
    } else {
        home->inj_head = f;
    }
    home->inj_tail = f;
    pthread_mutex_unlock(&home->mu);
    uint64_t one = 1;
    ssize_t w = write(home->doorbell, &one, sizeof(one));
    (void)w;
}

// Drain remote arrivals into the local queue. Owner-thread only.
static void neon_sched_drain_inj(void) {
    if (t_sched.inj_head == NULL) { // racy peek: a miss is caught by the doorbell
        return;
    }
    pthread_mutex_lock(&t_sched.mu);
    neon_fiber* f = t_sched.inj_head;
    t_sched.inj_head = NULL;
    t_sched.inj_tail = NULL;
    pthread_mutex_unlock(&t_sched.mu);
    while (f != NULL) {
        neon_fiber* next = f->q_next;
        neon_sched_enqueue_owned(f); // the injection push already claimed the queued bit
        f = next;
    }
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
    // Under a ring, readiness is a POLL_ADD completion like any other operation. It can
    // decline only when the submission queue is momentarily full, and then the epoll
    // registration below is the fallback for this one wait.
    if (neon_uring_active() && neon_uring_poll(fd, events)) {
        return;
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
    neon_sched_io_waiter_begin(); // mirrors into g_ext: the mesh's deadlock check must see us
    neon_fiber_park(); // the pump's epoll_wait wakes us when fd is ready
    neon_sched_io_waiter_end();
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

// The production trigger: a PER-THREAD CPU-time timer, one per scheduler seat, armed for the
// life of the runtime. CLOCK_THREAD_CPUTIME_ID + SIGEV_THREAD_ID (not the old process-wide
// ITIMER_VIRTUAL) is what makes preemption correct under M:N: each seat's timer ticks only
// while THAT thread burns CPU and the tick is delivered to THAT thread, whose thread-local
// safepoint flag it sets — so a hot loop on any seat is preempted, not just whichever
// thread happened to catch the one process timer. A seat idle in epoll_wait accrues no CPU
// time and is never interrupted for nothing. The signal disposition (SIGVTALRM, SA_RESTART
// so slow syscalls elsewhere resume rather than EINTR — epoll_wait is exempt by spec and
// handles EINTR in its own loop) is process-wide and installed once. 10ms: Go's quantum.
#if defined(__linux__)
#include <sys/syscall.h>
#include <time.h>

#define NEON_FIBER_QUANTUM_NS 10000000L

static _Thread_local timer_t t_timer;
static _Thread_local bool t_timer_armed;
static volatile sig_atomic_t g_alarm_installed;

static void neon_fiber_alarm(int sig) {
    (void)sig;
    t_preempt = 1; // thread-local: the tick lands on the seat whose CPU time it measured
}

static void neon_fiber_timer_arm(void) {
    if (!g_alarm_installed) {
        // The disposition is per-process; racing installs of the identical handler are
        // harmless, so no lock — the first mesh seat that gets here wins and the rest match.
        g_alarm_installed = 1;
        struct sigaction sa;
        memset(&sa, 0, sizeof(sa));
        sa.sa_handler = neon_fiber_alarm;
        sa.sa_flags = SA_RESTART;
        sigemptyset(&sa.sa_mask);
        sigaction(SIGVTALRM, &sa, NULL);
    }
    struct sigevent sev;
    memset(&sev, 0, sizeof(sev));
    sev.sigev_notify = SIGEV_THREAD_ID;
    sev.sigev_signo = SIGVTALRM;
    sev._sigev_un._tid = (int)syscall(SYS_gettid); // deliver to THIS seat's thread
    if (timer_create(CLOCK_THREAD_CPUTIME_ID, &sev, &t_timer) != 0) {
        return; // no per-thread timer here: cooperative-only, still correct, just not preemptive
    }
    t_timer_armed = true;
    struct itimerspec it;
    it.it_interval.tv_sec = 0;
    it.it_interval.tv_nsec = NEON_FIBER_QUANTUM_NS;
    it.it_value = it.it_interval;
    timer_settime(t_timer, 0, &it, NULL);
}

static void neon_fiber_timer_disarm(void) {
    if (t_timer_armed) {
        timer_delete(t_timer);
        t_timer_armed = false;
    }
    t_preempt = 0; // a tick that landed after the last safepoint must not leak out
}
#elif defined(__unix__)
#include <sys/time.h>
static void neon_fiber_alarm(int sig) { (void)sig; t_preempt = 1; }
static void neon_fiber_timer_arm(void) {
    struct sigaction sa; memset(&sa, 0, sizeof(sa));
    sa.sa_handler = neon_fiber_alarm; sa.sa_flags = SA_RESTART; sigemptyset(&sa.sa_mask);
    sigaction(SIGVTALRM, &sa, NULL);
    struct itimerval it; it.it_interval.tv_sec = 0; it.it_interval.tv_usec = 10000;
    it.it_value = it.it_interval; setitimer(ITIMER_VIRTUAL, &it, NULL); // process-wide fallback
}
static void neon_fiber_timer_disarm(void) {
    struct itimerval off; memset(&off, 0, sizeof(off)); setitimer(ITIMER_VIRTUAL, &off, NULL);
    t_preempt = 0;
}
#else
static void neon_fiber_timer_arm(void) {}
static void neon_fiber_timer_disarm(void) {}
#endif

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
