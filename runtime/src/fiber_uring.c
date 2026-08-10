// The io_uring backend: a COMPLETION engine under the same two seams the epoll reactor
// sits under (docs/design/fibers.md's slice 6). Where epoll answers "which descriptor is
// ready?" and leaves the syscall to the caller — which regular files cannot answer at all,
// hence the offload pool — io_uring takes the OPERATION itself: a read submitted here is
// performed by the kernel, and the fiber wakes with its result. Files need no threads, a
// batch of submissions costs one syscall, and the idle wait is the same call that reaps.
//
// Raw, with no liburing dependency: two syscalls (`io_uring_setup`, `io_uring_enter`) and
// three mmaps, against `<linux/io_uring.h>`'s ABI structs. That is deliberate — liburing is
// a convenience wrapper over exactly this, and the runtime ships as a static archive with
// no external deps.
//
// ---- what the rings are, and why the barriers are not optional ----
//
// Two single-producer/single-consumer rings shared with the kernel:
//
//   SQ: we produce (tail), the kernel consumes (head)   — submissions
//   CQ: the kernel produces (tail), we consume (head)   — completions
//
// The indices are plain shared memory, so ordering is ours to enforce. Two rules, and each
// one is a real bug if dropped:
//
//   * PUBLISHING a submission: fill the SQE, then RELEASE-store the new SQ tail. The
//     release is what guarantees the kernel cannot observe the tail bump before the SQE
//     contents — otherwise it reads a half-written entry (a garbage fd, a stale buffer
//     pointer) and performs it.
//   * CONSUMING a completion: ACQUIRE-load the CQ tail, then read the CQE. The acquire is
//     what guarantees the CQE fields we read are the ones the kernel wrote before it
//     bumped the tail — otherwise we can read a stale slot's `user_data` and wake the
//     wrong fiber, or a stale `res`.
//   * RELEASING a consumed completion: RELEASE-store the new CQ head, so the kernel does
//     not reuse a slot we have not finished reading.
//
// On x86-64 these compile to plain loads/stores plus a compiler barrier (TSO gives the
// rest); on aarch64 they are real `ldar`/`stlr`. Writing them as `__atomic_*` with explicit
// orderings is what makes the code correct on both rather than accidentally-correct on one.
//
// ---- how it plugs in ----
//
// Same seams as epoll, so nothing above the scheduler changes:
//   * operation hooks (`neon_fiber_blocking_read` / `_writev`) submit READ/WRITEV and park —
//     the OFFLOAD POOL IS BYPASSED ENTIRELY when a ring exists.
//   * `neon_fiber_io_wait(fd, events)` submits POLL_ADD (the pidfd wait, socket readiness).
//   * the pump's idle wait becomes `io_uring_enter(GETEVENTS)` with a TIMEOUT SQE carrying
//     the nearest sleep deadline.
//   * the cross-seat doorbell stays an eventfd, watched with a POLL_ADD like any other fd.
//
// Feature-detect at open: `io_uring_setup` returning ENOSYS (or a locked-down seccomp
// policy returning EPERM) leaves `ring.fd < 0` and every entry point answers "not here", so
// the scheduler keeps the epoll+offload path. `NEON_IO=epoll` in the environment forces the
// fallback, which is how both paths stay tested on a machine that has io_uring.

#include "libneon_rt.h"

#include "fiber_internal.h"
#include "internal.h"
#include "platform.h"

#if defined(__linux__)

#include <errno.h>
#include <linux/io_uring.h>
#include <poll.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

#define NEON_URING_ENTRIES 256

// One ring per scheduler seat: single-issuer by construction, which is why no lock guards
// any of this. The pointers are into the two mmap'd ring regions; `sqes` is the third.
typedef struct {
    int fd; // < 0 means "no ring here": every entry point below declines

    // SQ ring
    unsigned* sq_head;
    unsigned* sq_tail;
    unsigned* sq_mask;
    unsigned* sq_array; // index indirection: entry i of the ring names an SQE slot
    struct io_uring_sqe* sqes;
    unsigned sq_local_tail; // our private tail; published to *sq_tail at submit
    void* sq_map;
    size_t sq_map_len;

    // CQ ring
    unsigned* cq_head;
    unsigned* cq_tail;
    unsigned* cq_mask;
    struct io_uring_cqe* cqes;
    void* cq_map;
    size_t cq_map_len;

    void* sqe_map;
    size_t sqe_map_len;

    unsigned pending; // submitted-but-unreaped, so the pump knows a wait can produce work
} neon_uring;

// `fd` MUST start negative, and zero-initialization would make it 0 — a valid descriptor
// number, so every "do I have a ring?" test would answer yes on a seat that never opened
// one (and `std::sys::io_engine()` would report io_uring outside a runtime even under
// NEON_IO=epoll, which is exactly how this was found). The initializer is load-bearing.
static _Thread_local neon_uring t_ring = {.fd = -1};

// Reserved `user_data` tags. Never a fiber-request pointer (those are heap/stack addresses,
// always far above 2), so a completion is identified by value alone.
#define NEON_URING_TIMEOUT_TAG ((uint64_t)1)
#define NEON_URING_DOORBELL_TAG ((uint64_t)2)

static _Thread_local int t_doorbell_fd = -1;

static int neon_uring_setup(unsigned entries, struct io_uring_params* p) {
    return (int)syscall(SYS_io_uring_setup, entries, p);
}

static int neon_uring_enter(int fd, unsigned to_submit, unsigned min_complete, unsigned flags) {
    return (int)syscall(SYS_io_uring_enter, fd, to_submit, min_complete, flags, NULL, 0);
}

// Open this seat's ring. Returns false when io_uring is unavailable or disabled, and the
// caller then keeps the epoll path — a fallback, never a trap: an old kernel, a container
// policy, or NEON_IO=epoll are all ordinary environments, not errors.
bool neon_uring_open(void) {
    const char* forced = getenv("NEON_IO");
    if (forced != NULL && strcmp(forced, "epoll") == 0) {
        t_ring.fd = -1;
        return false;
    }
    struct io_uring_params p;
    memset(&p, 0, sizeof(p));
    int fd = neon_uring_setup(NEON_URING_ENTRIES, &p);
    if (fd < 0) {
        t_ring.fd = -1; // ENOSYS (pre-5.1), EPERM (seccomp), ENOMEM (locked memory limits)
        return false;
    }

    size_t sq_len = p.sq_off.array + p.sq_entries * sizeof(unsigned);
    size_t cq_len = p.cq_off.cqes + p.cq_entries * sizeof(struct io_uring_cqe);
    // IORING_FEAT_SINGLE_MMAP (5.4+) puts both rings in one mapping; without it they are
    // two. Handling both is three lines and keeps the floor at 5.1.
    if (p.features & IORING_FEAT_SINGLE_MMAP) {
        if (cq_len > sq_len) {
            sq_len = cq_len;
        }
        cq_len = sq_len;
    }
    void* sq = mmap(NULL, sq_len, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_POPULATE, fd,
                    IORING_OFF_SQ_RING);
    if (sq == MAP_FAILED) {
        close(fd);
        t_ring.fd = -1;
        return false;
    }
    void* cq = sq;
    if (!(p.features & IORING_FEAT_SINGLE_MMAP)) {
        cq = mmap(NULL, cq_len, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_POPULATE, fd,
                  IORING_OFF_CQ_RING);
        if (cq == MAP_FAILED) {
            munmap(sq, sq_len);
            close(fd);
            t_ring.fd = -1;
            return false;
        }
    }
    size_t sqe_len = p.sq_entries * sizeof(struct io_uring_sqe);
    void* sqes = mmap(NULL, sqe_len, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_POPULATE, fd,
                      IORING_OFF_SQES);
    if (sqes == MAP_FAILED) {
        if (cq != sq) {
            munmap(cq, cq_len);
        }
        munmap(sq, sq_len);
        close(fd);
        t_ring.fd = -1;
        return false;
    }

    t_ring.fd = fd;
    t_ring.sq_map = sq;
    t_ring.sq_map_len = sq_len;
    t_ring.cq_map = cq;
    t_ring.cq_map_len = cq_len;
    t_ring.sqe_map = sqes;
    t_ring.sqe_map_len = sqe_len;
    t_ring.sq_head = (unsigned*)((char*)sq + p.sq_off.head);
    t_ring.sq_tail = (unsigned*)((char*)sq + p.sq_off.tail);
    t_ring.sq_mask = (unsigned*)((char*)sq + p.sq_off.ring_mask);
    t_ring.sq_array = (unsigned*)((char*)sq + p.sq_off.array);
    t_ring.sqes = (struct io_uring_sqe*)sqes;
    t_ring.cq_head = (unsigned*)((char*)cq + p.cq_off.head);
    t_ring.cq_tail = (unsigned*)((char*)cq + p.cq_off.tail);
    t_ring.cq_mask = (unsigned*)((char*)cq + p.cq_off.ring_mask);
    t_ring.cqes = (struct io_uring_cqe*)((char*)cq + p.cq_off.cqes);
    t_ring.sq_local_tail = atomic_load_explicit((_Atomic unsigned*)t_ring.sq_tail,
                                                memory_order_relaxed);
    t_ring.pending = 0;

    // The SQ index array is a fixed identity mapping here: entry i always names SQE i. The
    // indirection exists for applications that reorder submissions; we never do, so writing
    // it once at open is correct and takes the write off the submit path.
    for (unsigned i = 0; i < p.sq_entries; i++) {
        t_ring.sq_array[i] = i;
    }
    return true;
}

void neon_uring_close(void) {
    if (t_ring.fd < 0) {
        return;
    }
    munmap(t_ring.sqe_map, t_ring.sqe_map_len);
    if (t_ring.cq_map != t_ring.sq_map) {
        munmap(t_ring.cq_map, t_ring.cq_map_len);
    }
    munmap(t_ring.sq_map, t_ring.sq_map_len);
    close(t_ring.fd);
    t_ring.fd = -1;
}

bool neon_uring_active(void) {
    return t_ring.fd >= 0;
}

// What `std::sys::io_engine()` reports. Inside a runtime this is THIS seat's answer (the
// ring it opened); outside one it is what a runtime WOULD open here, probed by opening a
// throwaway ring — the honest answer to "what would I get", at the cost of one setup+close
// on a call nobody makes in a loop.
bool neon_sys_uring_here(void) {
    if (t_ring.fd >= 0) {
        return true;
    }
    const char* forced = getenv("NEON_IO");
    if (forced != NULL && strcmp(forced, "epoll") == 0) {
        return false;
    }
    struct io_uring_params p;
    memset(&p, 0, sizeof(p));
    int fd = neon_uring_setup(8, &p);
    if (fd < 0) {
        return false;
    }
    close(fd);
    return true;
}

// Claim the next SQE. NULL when the ring is full — the caller then submits what it has and
// retries, which is the only backpressure this design needs (the queue is 256 deep and one
// fiber holds at most one in-flight operation).
static struct io_uring_sqe* neon_uring_sqe(void) {
    unsigned head = atomic_load_explicit((_Atomic unsigned*)t_ring.sq_head,
                                         memory_order_acquire);
    if (t_ring.sq_local_tail - head >= *t_ring.sq_mask + 1) {
        return NULL;
    }
    struct io_uring_sqe* sqe = &t_ring.sqes[t_ring.sq_local_tail & *t_ring.sq_mask];
    t_ring.sq_local_tail++;
    memset(sqe, 0, sizeof(*sqe));
    return sqe;
}

// Publish everything staged since the last publish, then enter the kernel. The RELEASE
// store is the barrier that matters: it orders every SQE write above it before the kernel
// can see the new tail (see the header comment).
static int neon_uring_submit(unsigned min_complete, bool wait) {
    unsigned tail = t_ring.sq_local_tail;
    unsigned old = atomic_load_explicit((_Atomic unsigned*)t_ring.sq_tail,
                                        memory_order_relaxed);
    unsigned to_submit = tail - old;
    if (to_submit > 0) {
        atomic_store_explicit((_Atomic unsigned*)t_ring.sq_tail, tail, memory_order_release);
    }
    if (to_submit == 0 && !wait) {
        return 0;
    }
    int r;
    do {
        r = neon_uring_enter(t_ring.fd, to_submit, min_complete,
                             wait ? IORING_ENTER_GETEVENTS : 0);
    } while (r < 0 && errno == EINTR);
    if (r > 0) {
        t_ring.pending += (unsigned)r;
    }
    return r;
}

// Reap every ready completion, waking each fiber with its result. The ACQUIRE load of the
// tail pairs with the kernel's release; the RELEASE store of the head hands slots back.
// A completion's `user_data` is the waiting fiber, except the timeout tag.
static void neon_uring_reap(void) {
    unsigned head = atomic_load_explicit((_Atomic unsigned*)t_ring.cq_head,
                                         memory_order_relaxed);
    for (;;) {
        unsigned tail = atomic_load_explicit((_Atomic unsigned*)t_ring.cq_tail,
                                             memory_order_acquire);
        if (head == tail) {
            break;
        }
        struct io_uring_cqe* cqe = &t_ring.cqes[head & *t_ring.cq_mask];
        uint64_t ud = cqe->user_data;
        int32_t res = cqe->res;
        head++;
        if (t_ring.pending > 0) {
            t_ring.pending--;
        }
        if (ud == NEON_URING_DOORBELL_TAG) {
            // A remote seat rang: drain the counter (the poll is level-triggered, so an
            // unread eventfd would fire again immediately) and re-arm for the next ring.
            // The pump's next lap drains the injection queue and re-checks g_done.
            uint64_t v;
            ssize_t r = read(t_doorbell_fd, &v, sizeof(v));
            (void)r;
            neon_uring_watch_doorbell(t_doorbell_fd);
            continue;
        }
        if (ud != NEON_URING_TIMEOUT_TAG && ud != 0) {
            // The waiting fiber's request block rides in user_data; store the result where
            // the parked fiber will read it, then admit the fiber.
            neon_uring_req* rq = (neon_uring_req*)(uintptr_t)ud;
            rq->res = res;
            rq->done = true;
            neon_fiber_wake(rq->fiber);
        }
    }
    atomic_store_explicit((_Atomic unsigned*)t_ring.cq_head, head, memory_order_release);
}

// ---- the operation entry points ----
//
// Each fills an SQE, parks the fiber, and returns the kernel's result. The request block
// lives on the PARKED FIBER'S OWN STACK — it stays live exactly as long as the fiber is
// parked, which is exactly as long as the completion can arrive.

static neon_ssize neon_uring_run(struct io_uring_sqe* sqe, neon_uring_req* rq) {
    rq->fiber = neon_fiber_current();
    rq->done = false;
    rq->res = 0;
    sqe->user_data = (uint64_t)(uintptr_t)rq;
    neon_sched_io_waiter_begin(); // an in-flight operation is external work, not a deadlock
    neon_uring_submit(0, false);  // publish; the pump's idle wait reaps
    neon_fiber_park();
    neon_sched_io_waiter_end();
    if (rq->res < 0) {
        errno = -rq->res;
        return -1;
    }
    return rq->res;
}

neon_ssize neon_uring_read(int fd, void* buf, size_t n) {
    struct io_uring_sqe* sqe = neon_uring_sqe();
    if (sqe == NULL) {
        neon_uring_submit(0, false); // ring full: drain and retry once
        sqe = neon_uring_sqe();
        if (sqe == NULL) {
            errno = EAGAIN;
            return -1;
        }
    }
    sqe->opcode = IORING_OP_READ;
    sqe->fd = fd;
    sqe->addr = (uint64_t)(uintptr_t)buf;
    sqe->len = (unsigned)n;
    sqe->off = (uint64_t)-1; // -1: use the file's current position, like read(2)
    neon_uring_req rq;
    return neon_uring_run(sqe, &rq);
}

neon_ssize neon_uring_writev(int fd, const neon_iovec* iov, int n) {
    struct io_uring_sqe* sqe = neon_uring_sqe();
    if (sqe == NULL) {
        neon_uring_submit(0, false);
        sqe = neon_uring_sqe();
        if (sqe == NULL) {
            errno = EAGAIN;
            return -1;
        }
    }
    sqe->opcode = IORING_OP_WRITEV;
    sqe->fd = fd;
    sqe->addr = (uint64_t)(uintptr_t)iov;
    sqe->len = (unsigned)n;
    sqe->off = (uint64_t)-1;
    neon_uring_req rq;
    return neon_uring_run(sqe, &rq);
}

// The cross-seat doorbell, as a ring registration. Without this an idle seat blocked in
// io_uring_enter cannot be reached by a remote wake — enter() knows nothing about an
// eventfd sitting in an epoll set — and a mesh whose work all lands on one seat deadlocks.
// It is submitted with its own tag rather than a fiber, re-armed after each firing, and
// drained by the reap loop, which reads the counter to disarm the level trigger.
void neon_uring_watch_doorbell(int fd) {
    if (t_ring.fd < 0) {
        return;
    }
    t_doorbell_fd = fd;
    struct io_uring_sqe* sqe = neon_uring_sqe();
    if (sqe == NULL) {
        return;
    }
    sqe->opcode = IORING_OP_POLL_ADD;
    sqe->fd = fd;
    sqe->poll32_events = POLLIN;
    sqe->user_data = NEON_URING_DOORBELL_TAG;
}

// Readiness, for the cases that are genuinely about readiness rather than a transfer: the
// pidfd a process wait blocks on, a socket handed to neon_fiber_io_wait.
bool neon_uring_poll(int fd, uint32_t events) {
    struct io_uring_sqe* sqe = neon_uring_sqe();
    if (sqe == NULL) {
        neon_uring_submit(0, false);
        sqe = neon_uring_sqe();
        if (sqe == NULL) {
            return false; // caller falls back to epoll for this one wait
        }
    }
    sqe->opcode = IORING_OP_POLL_ADD;
    sqe->fd = fd;
    sqe->poll32_events = events;
    neon_uring_req rq;
    neon_uring_run(sqe, &rq);
    return true;
}

// The pump's idle wait: block until a completion arrives or `timeout_ms` passes. A negative
// timeout waits indefinitely. The timeout rides as its own TIMEOUT SQE rather than as an
// enter() flag, which is what lets one call carry both — the whole point of a completion
// engine over a readiness one.
void neon_uring_wait(int timeout_ms) {
    if (timeout_ms >= 0) {
        struct io_uring_sqe* sqe = neon_uring_sqe();
        if (sqe != NULL) {
            // The timespec must outlive the call; it is read by the kernel during enter(),
            // and this frame is live for exactly that long.
            static _Thread_local struct __kernel_timespec ts;
            ts.tv_sec = timeout_ms / 1000;
            ts.tv_nsec = (long long)(timeout_ms % 1000) * 1000000;
            sqe->opcode = IORING_OP_TIMEOUT;
            sqe->addr = (uint64_t)(uintptr_t)&ts;
            sqe->len = 1;   // one timespec
            sqe->off = 0;   // fire on time, not after N completions
            sqe->user_data = NEON_URING_TIMEOUT_TAG;
        }
    }
    // min_complete 1: sleep until something lands — a completed operation, or the timeout.
    neon_uring_submit(1, true);
    neon_uring_reap();
}

// Non-blocking drain, for the pump's lap before it decides to idle.
void neon_uring_poll_completions(void) {
    neon_uring_submit(0, false);
    neon_uring_reap();
}

#else // !__linux__

bool neon_uring_open(void) { return false; }
void neon_uring_close(void) {}
bool neon_uring_active(void) { return false; }
void neon_uring_wait(int timeout_ms) { (void)timeout_ms; }
void neon_uring_poll_completions(void) {}
bool neon_uring_poll(int fd, uint32_t events) { (void)fd; (void)events; return false; }

#endif
