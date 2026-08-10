#include "libneon_rt.h"

#include <assert.h>

// ---- resources: body/ref with a baton (see neon/resource.h for the model) ----

// The transfer bracket. Set only around the three transfer edges' copies; read only by
// neon_wcopy_resource. Thread-local like the allocation routing: under M:N each scheduler
// thread runs one fiber at a time, and an edge's copy completes before the thread moves on.
static _Thread_local bool t_transfer;

void neon_transfer_begin(void) {
    assert(!t_transfer && "transfer edges never nest");
    t_transfer = true;
}

void neon_transfer_end(void) {
    t_transfer = false;
}

// The current execution context's identity for the owner slot. `neon_fiber_current`
// lazily adopts a per-thread root fiber and so never actually returns NULL today; the
// sentinel is a belt over those braces, because NULL in `owner` must mean exactly one
// thing — UNOWNED / in flight — and a context that ever identified as NULL would walk
// through the gate below (NULL == NULL) on someone else's in-flight resource.
#define NEON_RESOURCE_ROOT_CTX ((neon_fiber*)(uintptr_t)1)

static neon_fiber* neon_resource_ctx(void) {
    neon_fiber* f = neon_fiber_current();
    return f != NULL ? f : NEON_RESOURCE_ROOT_CTX;
}

// The body's own drop: memory death, NOT cleanup. Cleanup ran (or was deliberately
// skipped) before the last ref died; what remains is releasing the payload's counted
// parts — zeroed bytes when the payload was taken, which every witness release treats as
// the no-op it is (the take/finish contract, unchanged from the single-object design) —
// and the cleanup closure's shared-heap environment.
static void neon_resource_body_drop(void* p) {
    neon_resource_body* b = (neon_resource_body*)p;
    if (b->w->release) {
        b->w->release(neon_resource_body_payload(b));
    }
    if (b->cleanup.env) {
        neon_release(b->cleanup.env);
    }
    assert(b->header.rc == 0 && "a resource body was resurrected during its drop");
    neon_free(b);
}

// Mint a ref in the CURRENT context (the running fiber's arena, or the slab off-fiber),
// owning the reference the caller already holds on `body`. `drop` is the instantiation's
// emitted ref-drop, carried from ref to ref so the runtime can mint without knowing the
// payload type.
static neon_resource* neon_resource_ref_new(neon_resource_body* body, void (*drop)(void*),
                                            bool owning) {
    neon_resource* r =
        (neon_resource*)neon_alloc(sizeof(neon_resource) - sizeof(neon_header), drop);
    r->body = body;
    r->owning = owning;
    return r;
}

neon_resource* neon_resource_new(const void* payload, const neon_witness* w,
                                 neon_closure cleanup, void (*drop)(void*)) {
    // A capturing cleanup's environment crosses to the shared heap NOW, at construction —
    // the one moment the resource's author is on the stack to hear about an unsendable
    // capture — so cleanup can later run from whatever context drops the owning ref.
    if (cleanup.env != NULL && (cleanup.env->flags & NEON_ALLOC_ARENA)) {
        cleanup.env = neon_env_copy_to_shared(cleanup.env);
    }
    void* saved = neon_shared_routing_begin(); // the body's count is concurrent
    neon_resource_body* b = (neon_resource_body*)neon_alloc(
        sizeof(neon_resource_body) - sizeof(neon_header) + w->size, neon_resource_body_drop);
    b->w = w;
    b->cleanup = cleanup;
    b->armed = true;
    b->owner = neon_resource_ctx();
    // The payload's heap parts must not point into the creator's arena — the body may
    // outlive it. Deep-copy under the still-active shared routing; a payload that cannot
    // cross (a capturing closure inside it) traps here, at construction, not at some later
    // send. Scalars memcpy.
    if (w->copy) {
        w->copy(payload, neon_resource_body_payload(b));
    } else {
        memcpy(neon_resource_body_payload(b), payload, w->size);
    }
    neon_send_routing_end(saved);
    // The caller's original payload was CONSUMED by this native (the runtime-wide
    // convention); its counted parts are released in the caller's own context.
    if (w->release) {
        w->release((void*)payload);
    }
    return neon_resource_ref_new(b, drop, true);
}

void neon_resource_ref_finish(neon_resource* r) {
    neon_release_shared((neon_header*)r->body);
    assert(r->header.rc == 0 && "a resource ref was resurrected during its drop");
    neon_free(r);
}

bool neon_resource_take(neon_resource_body* b, void* out) {
    if (!b->armed) {
        return false;
    }
    b->armed = false;
    memcpy(out, neon_resource_body_payload(b), b->w->size);
    memset(neon_resource_body_payload(b), 0, b->w->size);
    return true;
}

// The gate every language-level use passes: is this fiber allowed to touch the body?
// An owning ref finding the body unowned is the far side of a transfer arriving — claim
// it. The claim is a plain store: the owning ref is unique, so exactly one fiber can be
// here, and it learned of the ref through the same publish edge the store rides back on.
static int64_t neon_resource_gate(neon_resource* r) {
    neon_resource_body* b = r->body;
    if (b->owner == NULL && r->owning) {
        b->owner = neon_resource_ctx();
    }
    if (b->owner != neon_resource_ctx()) {
        return NEON_RESOURCE_NOT_OWNER;
    }
    if (!b->armed) {
        return NEON_RESOURCE_RELEASED;
    }
    return NEON_RESOURCE_OK;
}

// These consume `r`, like every other native taking a counted pointer. Releasing may be
// the last reference, in which case the emitted drop runs right here — which is what
// last-use ARC means, and why the payload is retained/taken before that can happen.
int64_t neon_resource_get(neon_resource* r, void* out) {
    int64_t code = neon_resource_gate(r);
    if (code == NEON_RESOURCE_OK) {
        neon_resource_body* b = r->body;
        memcpy(out, neon_resource_body_payload(b), b->w->size);
        // The caller receives an owned value, like every other reader in this ABI.
        if (b->w->retain) {
            b->w->retain(out);
        }
    }
    neon_release((neon_header*)r);
    return code;
}

int64_t neon_resource_disarm(neon_resource* r, void* out) {
    int64_t code = neon_resource_gate(r);
    if (code == NEON_RESOURCE_OK) {
        bool took = neon_resource_take(r->body, out);
        assert(took && "gate said armed; take must agree");
        (void)took;
    }
    neon_release((neon_header*)r);
    return code;
}

// Hands back an owned closure, so its environment is retained before `r` goes: releasing
// `r` may be the last reference, and the body (and env) could die with it.
neon_closure neon_resource_cleanup(neon_resource* r) {
    neon_closure c = r->body->cleanup;
    if (c.env) {
        neon_retain(c.env);
    }
    neon_release((neon_header*)r);
    return c;
}

bool neon_resource_is_live(neon_resource* r) {
    bool live = r->body->armed;
    neon_release((neon_header*)r);
    return live;
}

// The witness copy for a resource slot (see the header). Officially `src` is const; a
// transfer demotes the source ref through the cast below — the honest price of
// transfer-as-copy, and the reason the header documents this copy as a baton edge.
void neon_wcopy_resource(const void* src, void* dst) {
    neon_resource* r = *(neon_resource* const*)src;
    neon_resource_body* b = r->body;
    neon_retain_shared((neon_header*)b);
    bool moving = t_transfer && r->owning;
    neon_resource* nr = neon_resource_ref_new(b, r->header.drop, moving);
    if (moving) {
        ((neon_resource*)r)->owning = false;
        b->owner = NULL; // in flight; the far side's first use claims it
    }
    *(neon_resource**)dst = nr;
}
