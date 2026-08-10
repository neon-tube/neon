// Model: the language channel's ring buffer, driven so that the doubling growth happens
// while the ring is WRAPPED (head != 0), then drained.
//
// THE INVARIANT: values come out of a channel in the order they went in, and the growth in
// `neon_channel_slot_reserve` preserves every buffered element, its bytes and its position
// in that order, even when the live run is split across the end of the old ring.
//
// Placeholder header - filled in after the run.

#include "../support/cbmc_support.h"
#include "libneon_rt.h"

#include "neon/channel.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int fprintf(FILE* stream, const char* fmt, ...) { (void)stream; (void)fmt; return 0; }
int fflush(FILE* stream) { (void)stream; return 0; }

// ---- the seams this model cuts, and nothing else ----

typedef struct neon_arena neon_arena;
_Thread_local neon_arena* neon_current_arena = NULL;
void* neon_shared_routing_begin(void) { return NULL; }

int pthread_mutex_init(pthread_mutex_t* m, const pthread_mutexattr_t* a) {
    (void)m; (void)a; return 0;
}
int pthread_mutex_destroy(pthread_mutex_t* m) { (void)m; return 0; }
int pthread_mutex_lock(pthread_mutex_t* m) { (void)m; return 0; }
int pthread_mutex_unlock(pthread_mutex_t* m) { (void)m; return 0; }

neon_fiber* neon_fiber_current(void) { return NULL; }
void neon_fiber_park(void) { }
void neon_fiber_wake(neon_fiber* f) { (void)f; }

// ---- the element: self-checking, and counted ----

#define ELEM_TAG(id) ((int64_t)(id) * 1000003 + 11)

typedef struct {
    int64_t id;
    int64_t tag; // == ELEM_TAG(id) for a well-formed element
} elem;

#define MAXID 9
static int g_live[MAXID]; // live owned instances of each id

static void elem_make(elem* e, int64_t id) {
    e->id = id;
    e->tag = ELEM_TAG(id);
    g_live[id]++;
}

static void w_copy(const void* src, void* dst) {
    const elem* s = (const elem*)src;
    PROVE(s->id > 0 && s->id < MAXID, "copy is handed an element this model made");
    PROVE(s->tag == ELEM_TAG(s->id), "copy is handed a well-formed element");
    elem* d = (elem*)dst;
    d->id = s->id;
    d->tag = s->tag;
    g_live[s->id]++;
}

static void w_release(void* p) {
    elem* e = (elem*)p;
    PROVE(e->id > 0 && e->id < MAXID, "release is handed an element this model made");
    PROVE(e->tag == ELEM_TAG(e->id), "release is handed a well-formed element");
    PROVE(g_live[e->id] > 0, "release is never handed an already-dead element");
    g_live[e->id]--;
}

static const neon_witness W = {sizeof(elem), NULL, w_release, NULL, NULL, w_copy};

// Codegen's shape for a consuming native: retain the handle, hand it over, the op eats it.
static void send_id(neon_channel_ref* r, int64_t id) {
    elem e;
    elem_make(&e, id);
    neon_retain((neon_header*)r);
    neon_channel_send(r, &e);
}

static int64_t recv_id(neon_channel_ref* r) {
    elem e;
    neon_retain((neon_header*)r);
    bool got = neon_channel_recv(r, &e);
    PROVE(got, "a receive on a non-empty channel returns a value");
    PROVE(e.tag == ELEM_TAG(e.id), "the received element is well-formed");
    w_release(&e); // the receiver consumes its copy
    return e.id;
}

int main(void) {
    neon_channel_ref* r = neon_channel_new(&W);

    // cap 0 -> 4 on the first send; the ring fills exactly.
    send_id(r, 1);
    send_id(r, 2);
    send_id(r, 3);
    send_id(r, 4);

    // Drain two: head is now 2, so the live run 3,4 sits at the END of the ring.
    PROVE(recv_id(r) == 1, "the first value out is the first value in");
    PROVE(recv_id(r) == 2, "the second value out is the second value in");

    // Refill to full: 5 and 6 land at indices 0 and 1, WRAPPED past head.
    send_id(r, 5);
    send_id(r, 6);

    // This one finds len == cap == 4 with head == 2 and grows to 8. The copy loop must
    // linearise 3,4,5,6 -- reading (head + i) % cap, not i -- and reset head to 0.
    send_id(r, 7);
    send_id(r, 8);

    PROVE(recv_id(r) == 3, "growth keeps the oldest value oldest");
    PROVE(recv_id(r) == 4, "growth preserves the run that sat at the end of the old ring");
    PROVE(recv_id(r) == 5, "growth preserves the run that had wrapped to the front");
    PROVE(recv_id(r) == 6, "growth preserves the order across the wrap boundary");
    PROVE(recv_id(r) == 7, "a value buffered after the growth follows the ones before it");
    PROVE(recv_id(r) == 8, "the last value in is the last value out");

    for (int64_t i = 1; i < MAXID; i++) {
        PROVE(g_live[i] == 0, "every value sent is owned exactly once end to end");
    }

    neon_release((neon_header*)r); // the last handle: the body and its ring go with it
    return 0;
}
