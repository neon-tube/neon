// Model: the explicit Neon-level `release` path -- fetch the closure, disarm, call it --
// with the resource dying between the fetch and the call.
//
// THE INVARIANT: `neon_resource_cleanup` RETAINS the closure environment before releasing
// the resource, so the environment of the closure it returns is still live when that
// closure is called.
//
// This is the one model in the set that catches removing that retain, and the window it
// exercises is narrow enough that nothing else does. `neon_resource_cleanup` consumes the
// resource, like every other native taking a counted pointer, and that release may be the
// last one -- in which case the drop runs, `neon_resource_finish` releases
// `r->cleanup.env`, and the environment dies with the resource that owned it. The closure
// the caller is holding then has a dangling `env` field, and the very next thing the
// emitted code does is call through it.
//
// The retain is what keeps the environment alive across that gap, and removing it is a
// use-after-free with no local symptom: the fetch succeeds, the disarm succeeds, and the
// call reads freed bytes. The sequence below is exactly what codegen emits for an explicit
// `release` -- retain once to pay for two consuming natives, fetch the closure, disarm,
// call -- so the check is on the real shape rather than a contrived one. It is harness,
// not code under test: every runtime function it calls is the real one.
//
// The counts are the assertions. `g_env->rc` is checked at each step, so the model fails
// on the *imbalance* rather than only if CBMC happens to notice a freed dereference, and
// it fails in both directions: a missing retain shows up as the environment already
// dropped before the call, an extra one as an environment never dropped at all.
//
// Rule 7 applies to the payload here too -- it is a COUNTED handle, a `neon_header*` with
// a real retain/release witness, never a scalar -- because this path also moves the
// payload out and hands it to a consuming cleanup, and with a scalar payload a double
// release of it would be a call through a NULL function pointer that never happens.
//
// Verifies `src/resource.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Baseline: 433 properties, VERIFICATION SUCCESSFUL. Two mutations, both reverted.
//
// 1. THE ONE THIS MODEL EXISTS FOR. The `if (c.env) neon_retain(c.env);` deleted from
//    `neon_resource_cleanup`, so the closure comes back borrowing an environment the
//    following release destroys. Failed on "neon_resource_cleanup retains the environment
//    BEFORE releasing the resource: the caller's closure now holds a reference of its
//    own", on "the environment outlives the resource that owned it, because the closure was
//    handed back owning a reference to it", on "the resource's reference to the environment
//    is gone and the closure's is what remains", and -- the payoff -- on "the environment
//    is still live when the closure is called, not bytes the resource's drop already
//    reclaimed", with deallocated-object dereferences on `env->rc` and `g_env->rc`
//    (9 of 433). It is the only model in the set that names this: every other resource
//    model stays green through it (0 failures on take-moves, disarm-picks, get-hands-back),
//    and `verify-resource-cleanup-runs-exactly-once` fails at 3 of 520 with nothing but
//    raw `neon_release` dereference failures -- a crash with no claim attached to it.
//    Shipped, this is a use-after-free with no local symptom: the fetch succeeds, the
//    disarm succeeds, and the call reads freed bytes.
//
// 2. `neon_resource_take` no longer clearing `armed`, so the drop runs the cleanup a
//    second time behind the explicit release. Failed on "the closure ran once, with a live
//    environment" (2 of 427) -- the model pins the call count, not only the environment's
//    liveness.
//
// NOT CAUGHT, and outside the claim: `neon_resource_get` losing its payload retain or
// gaining a disarm (0 of 433 each) -- this model never calls `get`. Deleting or doubling
// the payload release in `neon_resource_finish` (0 of 396, 0 of 452); see the note in
// `verify-resource-take-moves-the-payload-out-once` for why those are equivalent mutants.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. THE RESOURCE DIES INSIDE THE DISARM, not inside the `neon_resource_cleanup` call --
//    the harness holds two references and each native consumes one, exactly as emitted
//    code does. The narrower window, where `neon_resource_cleanup`'s own release is the
//    last one, is not driven here: an emitted explicit release always pays for both
//    natives up front, so reaching it would mean modelling a caller the compiler does not
//    generate. What is proved is that the environment survives from the fetch to the
//    call, which is the property the retain exists for either way.
//
// 2. A NULL ENVIRONMENT is not exercised. `neon_resource_cleanup` guards its retain with
//    `if (c.env)` and `neon_resource_finish` guards its release the same way, so a
//    captureless closure takes a different pair of branches. Those branches are trivially
//    balanced -- neither side does anything -- but "trivially" is an argument, not a
//    proof, and this model does not make it.
//
// 3. THE CLOSURE'S BODY. `c.fn` is called through the pointer, as codegen does, but what
//    a user's cleanup computes is not this file's business -- only that it is invoked
//    once, with an environment that is still live and a payload it owns.
//
// 4. ONE CONSUMING SEQUENCE, run once. Which operation ends a life, over all six public
//    entry points and from both states, is `verify-resource-cleanup-runs-exactly-once`.
//    The bound is a performance one and the expensive dimension is sequence DEPTH: a
//    `neon_release` CBMC cannot constant-fold leaves its object symbolically freed, and
//    the drop recursion behind it re-expands at every later dereference -- one extra
//    consuming operation ahead of the final one took the model this was split out of from
//    0.45s to over 300s. The two consuming natives here are affordable only because the
//    first is foldable (rc 2 -> 1, no drop).
//
// 5. Out-of-memory does not appear as a *return*: `neon_alloc` traps rather than returning
//    NULL. `--malloc-may-fail --malloc-fail-null` buys a check that the trap terminates
//    rather than running on with a NULL header, which the `_exit` stub encodes; a leak
//    check cannot fire past a trap.
//
// 6. Concurrency. The refcount is a plain `uint64_t` and the runtime is single-threaded.

#include "../support/cbmc_support.h"
#include "libneon_rt.h"

#include <stdio.h>

// Rule 4. `neon_trap` calls `fflush`/`fprintf`, the allocation check in `neon_alloc` can
// reach a trap under `--malloc-fail-null`, and CBMC's models of those pull a `FILE` into
// each of those sites. The model has nothing to say about stdio.
int fprintf(FILE* stream, const char* fmt, ...) { (void)stream; (void)fmt; return 0; }
int fflush(FILE* stream) { (void)stream; return 0; }

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

// The payload's witness: rule 7. One pointer, retain/release forwarding to the lifecycle
// -- the shape codegen emits for a `Resource[str, E]`.
static void handle_retain(void* elem) {
    neon_retain(*(neon_header**)elem);
}

static void handle_release(void* elem) {
    // The slot is zeroed by `neon_resource_take`, which `neon_resource_disarm` is written
    // in terms of, so on the moved-out path this is `neon_release(NULL)` -- a no-op.
    neon_release(*(neon_header**)elem);
}

static bool handle_eq(const void* a, const void* b) {
    return *(neon_header* const*)a == *(neon_header* const*)b;
}

static const neon_witness handle_witness = {
    .size = sizeof(neon_header*),
    .retain = handle_retain,
    .release = handle_release,
    .eq = handle_eq,
    .cmp = NULL,
};

// ---- the emitted, per-instantiation half ----

// A cleanup closure BORROWS its environment and CONSUMES its payload. Borrowing is what
// makes the retain in `neon_resource_cleanup` load-bearing: the closure never takes
// ownership of `env`, so nothing downstream would keep it alive.
typedef void (*cleanup_fn)(neon_header* env, neon_header* payload);

static void model_cleanup(neon_header* env, neon_header* payload) {
    PROVE(env != NULL, "cleanup receives its environment");
    // The check that fails when the retain is gone: by this point the resource that owned
    // the environment has been released, and without the retain these bytes are freed.
    PROVE(env->rc > 0,
          "the environment is still live when the closure is called, not bytes the "
          "resource's drop already reclaimed");
    PROVE(payload != NULL, "cleanup receives a payload");
    cleanup_calls++;
    neon_release(payload); // consumes the payload
}

// What codegen emits as the resource's `drop`: run cleanup if still armed, then land in
// the shared tail. Reached here from inside the disarm below, after that disarm has
// already taken the payload, so its own take fails.
static void model_drop(void* p) {
    neon_resource* r = (neon_resource*)p;
    neon_header* payload = NULL;
    if (neon_resource_take(r, &payload)) {
        ((cleanup_fn)r->cleanup.fn)(r->cleanup.env, payload);
    }
    neon_resource_finish(r);
}

// ---- the harness ----

// A distinguishable non-NULL address for the `out` slot, so "does not write out" is
// checked as *untouched* rather than "still NULL". Never dereferenced.
static neon_header sentinel_obj;
#define UNWRITTEN (&sentinel_obj)

int main(void) {
    neon_header* g_payload = (neon_header*)neon_alloc(0, payload_drop);
    neon_header* g_env = (neon_header*)neon_alloc(0, env_drop);

    neon_closure cleanup;
    cleanup.fn = (void*)model_cleanup;
    cleanup.env = g_env; // the resource takes ownership of this reference

    neon_resource* r = neon_resource_new(&g_payload, &handle_witness, cleanup, model_drop);

    // A reference the harness keeps and releases last, so a payload imbalance is caught as
    // a count rather than as a use-after-free.
    neon_retain(g_payload);

    PROVE(g_env->rc == 1, "the environment's only reference is the resource's");
    PROVE(r->header.rc == 1, "a fresh resource is uniquely owned");
    PROVE(r->armed, "a fresh resource is armed");

    // ---- the explicit release, in the shape codegen emits it ----
    //
    // Both `neon_resource_cleanup` and `neon_resource_disarm` consume a reference, so the
    // caller retains once to pay for the second. The net effect is one reference consumed
    // -- and since the harness holds only one, the disarm below is the last release and
    // the drop runs from inside it. That is the window this model exists to check.
    neon_retain((neon_header*)r);
    PROVE(r->header.rc == 2, "the explicit release path pays for two consuming natives");

    neon_closure c = neon_resource_cleanup(r); // consumes one reference

    PROVE(c.env == g_env, "the closure comes back with the environment it was built with");
    PROVE(c.fn == (void*)model_cleanup, "and with the function it was built with");
    PROVE(g_env->rc == 2,
          "neon_resource_cleanup retains the environment BEFORE releasing the resource: "
          "the caller's closure now holds a reference of its own");
    PROVE(r->header.rc == 1, "and consumed one reference to the resource");
    PROVE(env_drops == 0, "the environment is untouched by the fetch");

    neon_header* got = UNWRITTEN;
    bool mine = neon_resource_disarm(r, &got); // consumes the last one: the drop runs here

    PROVE(mine, "the explicit release won the disarm, so it owns the cleanup");
    PROVE(got == g_payload, "and was handed the payload that went in");

    // The resource is gone: its drop released the environment reference it owned. The
    // only thing keeping `c.env` alive is the retain `neon_resource_cleanup` took, which
    // is the whole claim.
    PROVE(env_drops == 0,
          "the environment outlives the resource that owned it, because the closure was "
          "handed back owning a reference to it");
    PROVE(g_env->rc == 1,
          "the resource's reference to the environment is gone and the closure's is what "
          "remains -- exactly the reference the retain took");

    // The call that is a use-after-free without that retain.
    ((cleanup_fn)c.fn)(c.env, got);
    PROVE(cleanup_calls == 1, "the closure ran once, with a live environment");

    neon_release(c.env); // the caller's owned reference, given back
    PROVE(env_drops == 1,
          "the environment is released exactly once overall: the retain is paid for, not "
          "a leak");

    PROVE(g_payload->rc == 1, "the payload was released exactly once, by the cleanup");
    PROVE(payload_drops == 0, "the payload is not dropped while the harness still holds it");

    neon_release(g_payload);
    PROVE(payload_drops == 1, "the payload is dropped exactly once, and only at rc == 0");

    // Nothing else is freed by hand. The resource and the environment must both have been
    // reclaimed by the code under test; --memory-leak-check is the assertion.
    return 0;
}
