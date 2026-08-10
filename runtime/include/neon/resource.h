#ifndef NEON_RESOURCE_H
#define NEON_RESOURCE_H

// Resources: an owned identity — a shared body, per-holder refs, and a baton.
//
// A resource is a payload plus a cleanup that runs exactly once. It is the one language
// value that does NOT copy when it crosses fibers: there is only one file descriptor, so
// crossing mints a fresh REF to the same BODY instead — the two-object split channels
// pioneered (see fiber_chan.c's "why a fiber's channel is TWO objects"), and for the same
// two reasons: refs are arena-resident, so a crashed fiber's teardown walk finds every
// holder by construction; and refs carry a plain thread-local count, so the generic
// retain/release path stays free of atomics. Only the body's count is concurrent, touched
// exactly at a ref's birth and death.
//
// ON TOP of the split sits OWNERSHIP — the baton. Exactly one fiber may use a resource
// (`owner`), and exactly one ref is the OWNING ref, whose death runs cleanup. The baton
// moves only at the three transfer edges (a `move` closure, a channel send, a task
// return), all of which route their copies through `neon_transfer_begin/end`; every other
// crossing mints a non-owning ref — the name travels, the authority does not. A transfer
// leaves the body UNOWNED (`owner == NULL`, in flight); the unique holder of the owning
// ref claims it at first use. Every write to `owner` therefore happens on the fiber
// holding the unique owning ref — a sequential handoff whose visibility rides the same
// publish edges the value itself crossed on — so none of this needs an atomic.
//
// Use from a non-owner fiber is a NAMED, CATCHABLE error at the language level (get
// reports a code; std::resource turns it into NotOwnerError/ReleasedError), never a trap:
// the failure modes this design removes are the silent ones — two fibers interleaving
// writes on one socket, or a double close on a reused descriptor number.
//
// The body lives on the SHARED heap: its payload is deep-copied there at construction
// (so a payload must be sendable when the resource is born — the price of a body that
// may outlive its creator), and a capturing cleanup's environment is copied there too,
// which is why cleanup can run from whatever context drops the owning ref, including a
// channel discarding an undelivered resource.

#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>

#include "neon/core.h"

typedef struct neon_fiber neon_fiber;

// `armed` and `owner` are RELAXED atomics, and the relaxation is the point: every write
// on the owning chain happens on the fiber holding the unique owning ref, with ordering
// supplied by the publish edge the value itself crossed on (a channel mutex, a task
// mutex, the spawn injection queue) — so no fence is needed here for correctness. The
// atomics exist for the OTHER readers: a non-owner fiber's gate check and a cross-fiber
// `is_live` race these fields by design, and a plain field would make that formal UB
// (and TSan noise) even though every hardware outcome is benign.
typedef struct neon_resource_body {
    neon_header header;    // concurrent count (SHARED heap); drop = neon_resource_body_drop
    const neon_witness* w; // payload witness
    neon_closure cleanup;  // env on the shared heap (copied at construction), or NULL env
    _Atomic bool armed;    // cleanup still owed
    _Atomic(neon_fiber*) owner; // the fiber allowed to use it; NULL = unowned / in flight
} neon_resource_body;

static inline void* neon_resource_body_payload(neon_resource_body* b) {
    return (void*)(b + 1);
}

// The per-holder handle: the Neon value `Resource[T, E]` IS this. `drop` (in the header)
// is the instantiation's emitted ref-drop: if this ref is OWNING and the body is still
// armed, it disarms, takes the typed payload and runs cleanup (failure discarded — a drop
// has no error channel), then lands in neon_resource_ref_finish either way.
typedef struct neon_resource {
    neon_header header;
    neon_resource_body* body;
    bool owning; // this ref carries the baton: its death runs cleanup
} neon_resource;

// Result codes for the gated reads, turned into thrown errors by std::resource.
enum {
    NEON_RESOURCE_OK = 0,
    NEON_RESOURCE_RELEASED = 1, // cleanup already ran (or payload taken)
    NEON_RESOURCE_NOT_OWNER = 2 // another fiber holds the baton (or it is in flight)
};

// Build a body on the shared heap (payload deep-copied there; a capturing cleanup's env
// copied there too) and return the OWNING ref, minted in the caller's context. `drop` is
// the instantiation's emitted ref-drop, copied onto every ref minted for this body.
neon_resource* neon_resource_new(const void* payload, const neon_witness* w,
                                 neon_closure cleanup, void (*drop)(void*));

// The shared tail of every emitted ref-drop: release this ref's count on the body and
// free the ref. Split out so the emitted drop is a few lines.
void neon_resource_ref_finish(neon_resource* r);

// Move the payload out of the body for a cleanup call: disarm first (whoever gets true
// owns the cleanup, and there is exactly one of them), copy the payload to `out`, zero it
// at the source so the body's own drop cannot release bytes whose ownership moved.
// UNGATED — callers are the emitted ref-drop (already the unique owner of the baton) and
// the gated disarm below.
bool neon_resource_take(neon_resource_body* b, void* out);

// The natives behind std::resource — each consumes its ref argument, the runtime-wide
// convention. The int64 returns are the NEON_RESOURCE_* codes above.
int64_t neon_resource_get(neon_resource* r, void* out);    // owned read; retains payload
int64_t neon_resource_disarm(neon_resource* r, void* out); // owned take, for release/take
bool neon_resource_is_live(neon_resource* r);
neon_closure neon_resource_cleanup(neon_resource* r); // owned closure, for release to call

// The witness copy for a resource slot: mint a ref in the destination context. Inside a
// transfer (neon_transfer_begin active) a copy of the OWNING ref moves the baton: the
// source ref is demoted, the minted ref is owning, and the body goes unowned until the
// far side's first use claims it. Outside a transfer, or from a non-owning source, the
// minted ref is non-owning.
void neon_wcopy_resource(const void* src, void* dst);

// Bracket the three transfer edges. Thread-local, like the allocation routing it rides
// beside; nesting is a bug (the edges never nest).
void neon_transfer_begin(void);
void neon_transfer_end(void);

#endif
