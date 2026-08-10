// Model: two wakes aimed at the same already-admitted fiber, against the real
// `neon_fiber_wake` and the real run queue behind it.
//
// THE INVARIANT: a fiber is admitted to a run queue at most once at a time. The second wake
// claims nothing and CHANGES NOTHING — not the fiber's queue link, not the link of the fiber
// behind it in the queue, not the tail the next admission will append to.
//
// Why it matters more than it looks. The run queue is INTRUSIVE: the "next" pointer lives in
// the fiber, so a fiber can be in the queue in only one place, and enqueuing one that is
// already there does not add a duplicate — it rewrites the queue. `neon_sched_enqueue` sets
// `f->q_next = NULL` and then hangs `f` off the tail, so re-enqueuing the fiber that is
// already at the tail makes it point at itself (`f->q_next == f`), and re-enqueuing one in
// the MIDDLE both truncates the queue at it and points its old successor backwards at it.
// The first is a pump that spins on one fiber forever; the second silently loses every fiber
// behind it — a hang whose cause is nowhere near where it shows up.
//
// Racing wakes are not exotic here, they are the normal case: `neon_channel_recv_timeout`
// exists precisely so a delivery and a deadline can both aim at one parked fiber, and
// `neon_fiber_wake`'s own comment calls the loser's wake "dissolving". The dissolving is a
// CAS on `f->queued` in `neon_sched_enqueue`, and this model is the check that it is load
// bearing rather than decorative.
//
// The queue is file-static in `src/fiber_sched.c` and cannot be read from a harness. It does
// not need to be: the corruption is entirely in fields of the FIBERS, which `fiber_internal.h`
// exposes, so three fibers and their `q_next` links pin the queue's shape exactly. (That is
// also the reason this model exists and a sleeper-list one does not — see SCOPE 3.)
//
// Verifies `src/fiber_sched.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Baseline: 194 properties, VERIFICATION SUCCESSFUL, 0.2s. Mutations applied to a COPY of
// `src/fiber_sched.c` (the shipping tree is never edited), each run against this harness:
//
// 1. THE BUG THIS MODEL IS FOR. `neon_sched_enqueue`'s claim
//        int zero = 0;
//        if (!atomic_compare_exchange_strong(&f->queued, &zero, 1)) return;
//    replaced by an unconditional `atomic_store(&f->queued, 1);`, so every wake admits.
//    Failed 3 of 206: "the re-woken fiber keeps its place in the queue" (f1 is torn out of
//    the middle and re-hung at the tail, so its link goes to NULL), "the fiber behind it is
//    not relinked ahead of it" (f2 now points BACKWARDS at f1) and "the tail is still where
//    the queue ended" (f3 is appended after f1, not after f2). Every fiber that was behind
//    f1 is unreachable from the head: the exact silent hang described above.
// 2. `t_sched.tail = f` dropped from the end of `neon_sched_enqueue`. Failed 3 of 194 on the
//    queue-shape properties: with the tail stuck at NULL every admission overwrites the head
//    instead of appending, so nothing is ever linked to anything.
// 3. The CAS's expected value flipped, `int zero = 0;` -> `int zero = 1;`, so a fresh fiber's
//    claim never succeeds. Failed 7 of 194, first on "a wake admits a parked fiber" — the
//    fiber is left BLOCKED and off the queue, which is a deadlock trap at the next idle pump.
// 4. `f->state = NEON_FIBER_READY` dropped from `neon_sched_enqueue`. Failed 2 of 188 on "a
//    wake admits a parked fiber": the fiber is on the queue but still reads BLOCKED, so the
//    pump's `else if (f->state == NEON_FIBER_BLOCKED)` arm drops it off the queue again the
//    moment it yields.
//
// NOT caught, and correctly so (scope, not blindness): dropping `f->q_next = NULL` from
// `neon_sched_enqueue`. Every fiber this harness enqueues is either fresh (already NULL) or
// refused by the CAS, so the stale link is never read; SUCCESSFUL, 188 of 188. Reaching it
// needs a dequeue, which is SCOPE 2.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. SEQUENTIAL, NOT INTERLEAVED. CBMC runs the two wakes one after the other; it does not
//    interleave them, so this is not a proof that the CAS is atomic — that is the C11
//    memory model's job, not ours. What it proves is the part we wrote: that admission is
//    GATED on the claim, and that a refused wake is inert. The genuinely concurrent form is
//    not reachable either: `neon_fiber_wake`'s remote arm needs a `neon_sched*`, a type
//    private to `src/fiber_sched.c`, and its local arm enqueues onto a `_Thread_local`
//    queue, so two racing threads would touch two different queues and no observable in any
//    fiber would differ.
// 2. NO DEQUEUE, SO NO RE-ADMISSION. `neon_sched_dequeue` — which clears the bit and is what
//    makes a later wake work at all — is static and reachable only from `neon_sched_pump`,
//    which needs epoll, io_uring, POSIX timers and the assembly context switch. This model
//    therefore covers "the bit refuses a second admission", not "the bit is released again".
//    The harness deliberately does NOT emulate the clear: a `atomic_store(&f->queued, 0)`
//    written here would be the harness asserting against itself.
// 3. THE LOCAL ARM ONLY. The fibers have `home == NULL` (`fiber_internal.h`: "NULL for
//    raw-primitive fibers driven outside a scheduler"), which takes the same branch as
//    `home == &t_sched`. The remote arm — injection queue, doorbell — is unreachable for the
//    reason in note 1, and it has its OWN copy of the CAS, which is consequently unverified.
// 4. THE SLEEPER-CANCEL SCAN IS EXERCISED EMPTY. `neon_fiber_wake` scans `t_sched.sleepers`
//    before admitting, and this harness has no sleepers, so the scan runs zero iterations.
//    Populating it means calling `neon_fiber_park_deadline`, which parks through
//    `neon_fiber_yield` — the assembly context switch. Not covered here, and not covered
//    anywhere: see the report accompanying this model.

#include "../support/cbmc_support.h"
#include "libneon_rt.h"

// The fiber struct and its scheduler-facing fields. src-private (never installed), and the
// only place `queued`, `q_next` and `state` are declared — which is what makes the queue's
// shape observable from a harness at all.
#include "../../src/fiber_internal.h"

#include <stdio.h>
#include <stdlib.h>

int fprintf(FILE* stream, const char* fmt, ...) { (void)stream; (void)fmt; return 0; }
int fflush(FILE* stream) { (void)stream; return 0; }

// The scheduler's `mu` guards the injection queue and the sleeper list against OTHER
// threads; a single-threaded model needs no mutual exclusion, and stubbing it keeps CBMC's
// budget on the queue rather than on glibc's lock (rule 4).
int pthread_mutex_init(pthread_mutex_t* m, const pthread_mutexattr_t* a) {
    (void)m; (void)a; return 0;
}
int pthread_mutex_destroy(pthread_mutex_t* m) { (void)m; return 0; }
int pthread_mutex_lock(pthread_mutex_t* m) { (void)m; return 0; }
int pthread_mutex_unlock(pthread_mutex_t* m) { (void)m; return 0; }

// Three parked fibers. Static, so they start as `neon_fiber_new` leaves a fresh one: state
// READY(0), `queued` 0, `q_next` NULL, and `home` NULL — the raw-primitive placement, which
// takes `neon_fiber_wake`'s local arm (SCOPE 3).
static neon_fiber f1, f2, f3;

int main(void) {
    f1.state = NEON_FIBER_BLOCKED;
    f2.state = NEON_FIBER_BLOCKED;
    f3.state = NEON_FIBER_BLOCKED;

    // A queue of two: head -> f1 -> f2 -> NULL.
    neon_fiber_wake(&f1);
    PROVE(f1.state == NEON_FIBER_READY, "a wake admits a parked fiber");
    PROVE(atomic_load(&f1.queued) == 1, "an admitted fiber holds the queued bit");
    PROVE(f1.q_next == NULL, "the first fiber in the queue is also the last");

    neon_fiber_wake(&f2);
    PROVE(f1.q_next == &f2, "the second admission is linked behind the first");
    PROVE(f2.q_next == NULL, "and is the new end of the queue");

    // THE RACE, sequentialised: a second waker aims at f1, which is still queued and now
    // sits in the MIDDLE of the queue. Everything below must be exactly as it was.
    neon_fiber_wake(&f1);

    PROVE(atomic_load(&f1.queued) == 1, "the queued bit still records exactly one admission");
    PROVE(f1.state == NEON_FIBER_READY, "the refused wake does not restate the fiber");
    PROVE(f1.q_next == &f2, "the re-woken fiber keeps its place in the queue");
    PROVE(f2.q_next == NULL, "the fiber behind it is not relinked ahead of it");

    // ...and the tail the NEXT admission appends to is still f2, not f1.
    neon_fiber_wake(&f3);
    PROVE(f2.q_next == &f3, "the tail is still where the queue ended");
    PROVE(f3.q_next == NULL, "the third admission is the new end of the queue");

    return 0;
}
