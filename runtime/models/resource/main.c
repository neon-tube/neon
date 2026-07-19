// Model: resources -- arm/disarm, move-out, and the exactly-once cleanup.
//
// Drives `neon_resource_new` / `_take` / `_disarm` / `_get` / `_cleanup` / `_is_live` /
// `_finish` from `src/resource.c` -- the shipping source, compiled by CBMC alongside this
// harness, never a copy -- through every interleaving of the operations up to a bound.
//
// ---- The payload is counted, on purpose ----
//
// This code use-after-freed on 2026-07-19: an emitted drop moved the payload out and
// handed it to a cleanup that consumes it, and then `neon_resource_finish` released the
// same bytes a second time. Nothing caught it, because every `Resource[...]` in the tree
// held a scalar, and a scalar's witness has no `release` -- so the second release was a
// call through a NULL function pointer that never happened. The first `Resource[str, E]`
// found it.
//
// So the payload here is a *counted handle*: a `neon_header*` whose witness retains and
// releases it. Every double-release of the payload slot is therefore a real second
// `neon_release` on a real object, which CBMC sees as a deallocated-pointer dereference
// (the object is `free`d at rc == 0) or as a drop count of 2. A model with a scalar
// payload would pass and prove nothing; that is the whole point of this file.
//
// ---- What is proved ----
//
// Over every sequence of up to three consuming operations on a resource held by up to
// three references, in every order, with the last release running the emitted drop:
//
//   - cleanup runs EXACTLY ONCE, across every interleaving of take / disarm / get /
//     cleanup / is_live / release and the drop, and whichever of them wins the race;
//   - the payload object is dropped exactly once -- no double free, no leak, and no
//     use-after-free from `neon_resource_finish` releasing bytes whose ownership moved;
//   - `neon_resource_take` on an already-disarmed resource returns false and does not
//     write `out`, so the second caller does not get the payload;
//   - the same for `neon_resource_disarm`, which is the disarm-first safety property:
//     of every caller, exactly one is told it owns the cleanup;
//   - the closure environment is retained by `neon_resource_cleanup` before the
//     resource is released (else the explicit-release path below is a use-after-free)
//     and released exactly once overall;
//   - `neon_resource_is_live` agrees with the harness's own tracking of the flag;
//   - `neon_resource_get` hands back an owned payload and does not disarm;
//   - the resource itself is freed exactly once (--memory-leak-check);
//   - `neon_resource_finish`'s resurrection assertion holds on every path.
//
// The `model_drop` / `explicit_release` pair below is what codegen emits per
// instantiation, in the shape codegen emits it. It is harness, not code under test: the
// runtime functions it calls are the real ones. Substituting the buggy 2026-07-19 drop
// (drop the `neon_resource_take` and read the payload slot directly) makes this model
// fail with a double free, which is the check that it has teeth.
//
// ---- What is deliberately NOT proved ----
//
//   - Out-of-memory does not appear as a *return* anywhere. `neon_alloc` traps rather
//     than returning NULL, so `neon_resource_new` has no failure path to model; what
//     `--malloc-may-fail --malloc-fail-null` buys here is a check that the trap
//     terminates instead of running on with a NULL header, which the `_exit` stub in
//     cbmc_support.h encodes.
//   - Concurrency. The refcount is a plain `uint64_t` and the runtime is
//     single-threaded; "interleaving" above means sequential orderings of the
//     operations, not simultaneous ones.
//   - Refcount overflow at 2^64. Unreachable in any finite execution.
//   - The closure's *body*. `cleanup.fn` is called through the pointer, as codegen does,
//     but what a user's cleanup computes is not this file's business -- only that it is
//     invoked once, with the payload it owns.
//
// ---- Assumptions (each one is a hole; there are three, all bounding) ----
//
// Every `ASSUME` / `NONDET_UPTO` below bounds a loop or an operation tag. None of them
// excludes a behaviour of the code under test; see the comment at each use.

#include "../support/cbmc_support.h"
#include "libneon_rt.h"

// ---- a counted payload, and a counted closure environment ----

static unsigned payload_drops;
static unsigned env_drops;
static unsigned cleanup_calls;

static void payload_drop(void* p) {
    payload_drops++;
    neon_free(p);
}

static void env_drop(void* p) {
    env_drops++;
    neon_free(p);
}

// The payload's witness. `size` is one pointer, and retain/release forward to the
// lifecycle -- this is exactly the shape codegen emits for `Resource[str, E]`, whose
// payload is a counted `neon_str` owner.
static void handle_retain(void* elem) {
    neon_retain(*(neon_header**)elem);
}
static void handle_release(void* elem) {
    // The slot is zeroed by `neon_resource_take`, so on the moved-out path this is
    // `neon_release(NULL)` -- a no-op. If the zeroing ever goes away, this becomes a
    // second release of a live object and the drop counts below catch it.
    neon_release(*(neon_header**)elem);
}
static bool handle_eq(const void* a, const void* b) {
    return *(neon_header* const*)a == *(neon_header* const*)b;
}

static const neon_witness handle_witness = {
    sizeof(neon_header*), handle_retain, handle_release, handle_eq, NULL,
};

// ---- the emitted, per-instantiation half ----

// A cleanup closure: borrows its environment, CONSUMES its payload. Consuming the payload
// is the case that broke -- a cleanup that closes a handle and releases it is the normal
// shape, not an exotic one.
typedef void (*cleanup_fn)(neon_header* env, neon_header* payload);

static void model_cleanup(neon_header* env, neon_header* payload) {
    PROVE(env != NULL, "cleanup receives its environment");
    cleanup_calls++;
    neon_release(payload); // consumes the payload
}

// What codegen emits as the resource's `drop`. Runs cleanup if still armed, then lands in
// the shared tail. The `neon_resource_take` is load-bearing: without it the payload is
// released here and again in `neon_resource_finish`.
static void model_drop(void* p) {
    neon_resource* r = (neon_resource*)p;
    neon_header* payload = NULL;
    if (neon_resource_take(r, &payload)) {
        PROVE(payload != NULL, "a successful take yields the payload");
        ((cleanup_fn)r->cleanup.fn)(r->cleanup.env, payload);
    }
    neon_resource_finish(r);
}

// What the explicit Neon-level `release` compiles to: take an owned copy of the closure,
// disarm, and call it. Both natives consume a reference, so the caller retains once to
// pay for the second; net effect is one reference consumed.
//
// This is the shape that catches a missing retain in `neon_resource_cleanup`: if the
// closure environment were handed back unretained, the `neon_resource_disarm` below could
// be the last release and `c.env` would be dangling by the time it is called.
static void explicit_release(neon_resource* r) {
    neon_retain((neon_header*)r);
    neon_closure c = neon_resource_cleanup(r); // consumes one ref; c.env is owned
    neon_header* got = NULL;
    bool mine = neon_resource_disarm(r, &got); // consumes the other
    if (mine) {
        PROVE(got != NULL, "a successful disarm yields the payload");
        ((cleanup_fn)c.fn)(c.env, got);
    } else {
        PROVE(got == NULL, "disarm on a disarmed resource leaves out untouched");
    }
    neon_release(c.env);
}

// ---- the harness ----

int main(void) {
    neon_header* payload = (neon_header*)neon_alloc(0, payload_drop);
    neon_header* env = (neon_header*)neon_alloc(0, env_drop);

    neon_closure cleanup;
    cleanup.fn = (void*)model_cleanup;
    cleanup.env = env; // the resource takes ownership of this reference

    // The payload's single reference moves into the resource.
    neon_resource* r = neon_resource_new(&payload, &handle_witness, cleanup, model_drop);

    PROVE(r->header.rc == 1, "a fresh resource has rc == 1");
    PROVE(r->armed, "a fresh resource is armed");
    PROVE(*(neon_header**)neon_resource_payload(r) == payload,
                     "the payload is copied into the inline slot");

    // ASSUMPTION 1: at most two extra references, bounding the operation loop below. A
    // *size* bound, not a behavioural one; the reason is spelled out in the call.
    unsigned extra = NONDET_UPTO(
        2,
        "the state space of a resource is (armed, rc > 0); three references already reach "
        "every ordering of every operation against both values of `armed` -- a take before "
        "the drop, a take after one, and a drop that finds the resource already disarmed. "
        "A fourth reference permutes the same states rather than adding one.");
    for (unsigned i = 0; i < extra; i++) {
        neon_retain((neon_header*)r);
    }
    unsigned refs = 1 + extra; // <= 3, well under --unwind 12
    PROVE(r->header.rc == refs, "rc tracks retains");

    bool expect_armed = true;

    // Each iteration consumes exactly one reference, so the last one runs `model_drop`.
    for (unsigned i = 0; i < refs; i++) {
        // ASSUMPTION 2: `op` names one of the six operations. A pure encoding
        // assumption: it removes no behaviour, because tags above 5 name nothing.
        unsigned op = NONDET_UPTO(
            5,
            "an operation tag, not a restriction: 0-5 enumerate every public entry point "
            "that consumes a reference (take+release, explicit release, get, cleanup, "
            "is_live, bare release), so the loop reaches every sequence of them. Values "
            "above 5 denote no operation at all.");

        if (op == 0) {
            // Bare `take` -- the move-out on its own, which is how the drop path and any
            // emitted "take the payload and clean it up here" both use it. Non-consuming,
            // so a plain release pays for the reference.
            neon_header* got = NULL;
            bool mine = neon_resource_take(r, &got);
            PROVE(mine == expect_armed, "take succeeds iff armed");
            if (mine) {
                PROVE(got == payload, "take yields the payload that went in");
                PROVE(!r->armed, "take disarms");
                PROVE(*(neon_header**)neon_resource_payload(r) == NULL,
                                 "take zeroes the source slot");
                expect_armed = false;
                // We now own the payload, so we owe it a cleanup, exactly as the emitted
                // code would. Borrowing `r->cleanup.env` is safe: we still hold a ref.
                ((cleanup_fn)r->cleanup.fn)(r->cleanup.env, got);
            } else {
                PROVE(got == NULL, "take on a disarmed resource leaves out untouched");
            }
            neon_release((neon_header*)r);

        } else if (op == 1) {
            // The explicit release path.
            bool was_armed = expect_armed;
            unsigned before = cleanup_calls;
            expect_armed = false;
            explicit_release(r);
            PROVE(cleanup_calls == before + (was_armed ? 1u : 0u),
                             "explicit release cleans up iff it won the disarm");

        } else if (op == 2) {
            // `get`: an owned read that must NOT disarm.
            neon_header* got = NULL;
            bool live = neon_resource_get(r, &got);
            PROVE(live == expect_armed, "get reports liveness");
            if (live) {
                PROVE(got == payload, "get yields the payload");
                neon_release(got); // the read was owned; give it back
            } else {
                PROVE(got == NULL, "get on a released resource leaves out untouched");
            }
            // `expect_armed` is unchanged: get does not disarm. If it ever did, the
            // liveness assertion above fires on the next iteration.

        } else if (op == 3) {
            // The closure getter on its own.
            neon_closure c = neon_resource_cleanup(r);
            PROVE(c.env == env, "the closure comes back intact");
            neon_release(c.env); // it was handed to us retained

        } else if (op == 4) {
            bool live = neon_resource_is_live(r);
            PROVE(live == expect_armed, "is_live agrees with the armed flag");

        } else {
            neon_release((neon_header*)r);
        }
    }

    // The last release above ran `model_drop`, so everything is settled.
    PROVE(cleanup_calls == 1, "cleanup runs exactly once, on every interleaving");
    PROVE(payload_drops == 1, "the payload is dropped exactly once");
    PROVE(env_drops == 1, "the closure environment is released exactly once");

    // Nothing is freed here: the resource, the payload and the environment must all have
    // been reclaimed by the code under test. --memory-leak-check is the assertion.
    return 0;
}
