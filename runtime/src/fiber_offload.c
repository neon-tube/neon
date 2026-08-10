// The file-offload pool (docs/design/fibers.md): regular files cannot be waited on with a
// readiness API — epoll rejects them, and a disk read blocks for real — so blocking file
// syscalls are handed to a small pthread pool and the calling FIBER parks until the worker
// finishes. The workers run raw syscalls only: they never touch a neon object, a refcount,
// or an arena, which is what makes a thread pool sound inside the M=1 runtime — all fiber
// and heap machinery stays on the scheduler thread.
//
// The round trip: the hook builds a request ON THE CALLING FIBER'S STACK, queues it
// (mutex+cond), and parks; a worker pops it, performs the syscall (retrying EINTR), writes
// the result back into the request, pushes it onto the completion list, and rings an
// eventfd; the scheduler's epoll sees the eventfd (a persistent registration with this
// file's tag), drains completions, and wakes each fiber — which reads its result off its
// own stack. Off-fiber (the root context, or a program with no runtime) the hooks fall
// through to the plain blocking call, unchanged.

#include "libneon_rt.h"

#include "fiber_internal.h"
#include "internal.h"
#include "platform.h"

#if defined(__linux__)

#include <errno.h>
#include <pthread.h>
#include <stdlib.h>
#include <sys/eventfd.h>
#include <unistd.h>

enum { NEON_OFFLOAD_READ, NEON_OFFLOAD_WRITEV };

typedef struct neon_offload_req {
    int op;
    int fd;
    void* buf;             // read: destination
    size_t n;              // read: capacity
    const neon_iovec* iov; // writev: the batch
    int iovn;
    neon_ssize result;
    int err; // errno from the worker, restored for the caller
    neon_fiber* fiber;
    struct neon_offload_req* next;
} neon_offload_req;

// One pool per process (M=1: one scheduler thread talks to it). Two workers: file IO is
// latency-hiding here, not bandwidth-scaling, and the design's proactor replaces all this.
#define NEON_OFFLOAD_WORKERS 2

static pthread_mutex_t g_mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t g_cv = PTHREAD_COND_INITIALIZER;
static neon_offload_req* g_qhead;
static neon_offload_req* g_qtail;
static neon_offload_req* g_done; // completed, awaiting the scheduler's drain
static int g_eventfd = -1;
static bool g_started;

char neon_offload_tag; // the epoll data.ptr the scheduler recognises as "drain completions"

static void* neon_offload_worker(void* arg) {
    (void)arg;
    for (;;) {
        pthread_mutex_lock(&g_mu);
        while (g_qhead == NULL) {
            pthread_cond_wait(&g_cv, &g_mu);
        }
        neon_offload_req* r = g_qhead;
        g_qhead = r->next;
        if (g_qhead == NULL) {
            g_qtail = NULL;
        }
        pthread_mutex_unlock(&g_mu);

        neon_ssize got;
        do {
            got = r->op == NEON_OFFLOAD_READ ? neon_plat_read(r->fd, r->buf, r->n)
                                             : neon_plat_writev(r->fd, r->iov, r->iovn);
        } while (got < 0 && errno == EINTR);
        r->result = got;
        r->err = got < 0 ? errno : 0;

        pthread_mutex_lock(&g_mu);
        r->next = g_done;
        g_done = r;
        pthread_mutex_unlock(&g_mu);
        uint64_t one = 1;
        ssize_t w = write(g_eventfd, &one, sizeof(one));
        (void)w;
    }
    return NULL;
}

// Scheduler-thread only: called from the pump when the eventfd fires.
void neon_offload_drain(void) {
    uint64_t n;
    ssize_t r = read(g_eventfd, &n, sizeof(n));
    (void)r;
    pthread_mutex_lock(&g_mu);
    neon_offload_req* done = g_done;
    g_done = NULL;
    pthread_mutex_unlock(&g_mu);
    while (done != NULL) {
        neon_offload_req* next = done->next;
        neon_fiber_wake(done->fiber);
        done = next;
    }
}

static void neon_offload_start(void) {
    if (g_started) {
        return;
    }
    g_started = true;
    g_eventfd = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
    if (g_eventfd < 0) {
        neon_trap("eventfd failed");
    }
    neon_sched_epoll_add_tag(g_eventfd, &neon_offload_tag); // persistent, pump-recognised
    for (int i = 0; i < NEON_OFFLOAD_WORKERS; i++) {
        pthread_t t;
        if (pthread_create(&t, NULL, neon_offload_worker, NULL) != 0) {
            neon_trap("offload worker thread failed to start");
        }
        pthread_detach(t);
    }
}

// Queue the request, park the fiber, return the syscall's result with errno restored.
static neon_ssize neon_offload_run(neon_offload_req* r) {
    neon_offload_start();
    r->fiber = neon_fiber_current();
    r->next = NULL;
    pthread_mutex_lock(&g_mu);
    if (g_qtail != NULL) {
        g_qtail->next = r;
    } else {
        g_qhead = r;
    }
    g_qtail = r;
    pthread_cond_signal(&g_cv);
    pthread_mutex_unlock(&g_mu);
    neon_sched_io_waiter_begin(); // an offloaded fiber is an IO waiter, not a deadlock
    neon_fiber_park();
    neon_sched_io_waiter_end();
    errno = r->err;
    return r->result;
}

// The hooks internal.h declares and file.c consults. On the root context — a resource
// cleanup, say — fall through to the plain blocking call: there is no fiber to park.
static neon_ssize neon_offload_read_hook(int fd, void* buf, size_t n) {
    if (neon_fiber_current()->is_root) {
        return neon_plat_read(fd, buf, n);
    }
    neon_offload_req r = {NEON_OFFLOAD_READ, fd, buf, n, NULL, 0, 0, 0, NULL, NULL};
    return neon_offload_run(&r);
}

static neon_ssize neon_offload_writev_hook(int fd, const neon_iovec* iov, int n) {
    if (neon_fiber_current()->is_root) {
        return neon_plat_writev(fd, iov, n);
    }
    neon_offload_req r = {NEON_OFFLOAD_WRITEV, fd, NULL, 0, iov, n, 0, 0, NULL, NULL};
    return neon_offload_run(&r);
}

// Armed and disarmed by the scheduler with the other hooks (fiber_sched.c).
void neon_offload_arm(void) {
    neon_fiber_blocking_read = neon_offload_read_hook;
    neon_fiber_blocking_writev = neon_offload_writev_hook;
}

void neon_offload_disarm(void) {
    neon_fiber_blocking_read = NULL;
    neon_fiber_blocking_writev = NULL;
}

#else // !__linux__

void neon_offload_drain(void) {}
void neon_offload_arm(void) {}
void neon_offload_disarm(void) {}
char neon_offload_tag;

#endif
