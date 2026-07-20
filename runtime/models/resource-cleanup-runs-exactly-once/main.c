// Model: every public operation that can end a resource's life, run as the last one, and
// the cleanup call each of them does or does not make.
//
// THE INVARIANT: cleanup runs EXACTLY ONCE over a resource's life -- from the armed state
// and from the disarmed state, whichever public operation takes the last reference.
//
// This is the property the armed flag exists for. Cleanup has two entry points that know
// nothing about each other: the explicit Neon-level `release`, which disarms and calls the
// closure so the error propagates, and the emitted drop on the last reference, which has
// no error channel. Both can run for the same resource in the same execution -- an
// explicit release whose reference was not the last one, a drop after something already
// took the payload -- and the failure modes on either side are silent. Twice is a second
// `close` on a descriptor the OS has already reused; zero times is a handle leak that no
// test observes because the program exits anyway.
//
// Rule 7 is why the payload is a COUNTED handle -- a `neon_header*` with a real
// retain/release witness -- and not a scalar. Every `Resource[...]` in the tree held a
// scalar when this code use-after-freed, and a scalar's witness has no `release`, so the
// second release was a call through a NULL function pointer that never happened. With a
// counted payload a double cleanup is a real second `neon_release` on a real object, and
// the reference count check at the end of each scenario catches it in both directions:
// `rc == 0` is one release too many, `rc == 2` is a leak.
//
// The `model_drop` pair below is what codegen emits per instantiation, in the shape
// codegen emits it. It is harness, not code under test: every runtime function it calls is
// the real one.
//
// Verifies `src/resource.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Baseline: 520 properties, VERIFICATION SUCCESSFUL. Five mutations, each reverted.
//
// 1. THE HISTORICAL BUG, reproduced. `model_drop` changed to read the payload slot
//    directly -- `payload = *(neon_header**)neon_resource_payload(r)` guarded by
//    `r->armed` -- and hand those bytes to the consuming cleanup, instead of calling
//    `neon_resource_take` first. The slot is never zeroed, so `neon_resource_finish`
//    releases the same bytes the cleanup already released. Failed on "the payload was
//    released exactly once, by whoever ran the cleanup" and "the payload is not dropped
//    while the harness still holds it", plus three deallocated-object dereferences inside
//    `neon_release` (6 of 538). This is the shape that shipped: it ran clean against every
//    `Resource[i64, E]` in the tree, because a scalar witness has no `release` and the
//    second call went through a NULL pointer, and use-after-freed the first
//    `Resource[str, E]`. The mutation is in the harness because the defect was in emitted
//    code, not in the runtime.
//
// 2. `neon_resource_take` no longer clearing `armed`. Failed on "cleanup runs exactly once
//    over the resource's life, from either state and whichever operation ended it", and on
//    four more including "take wins the cleanup if and only if the resource is armed"
//    (5 of 514). Shipped, this is a second `close` on a descriptor the OS has already
//    handed to someone else.
//
// 3. `neon_resource_get` additionally disarming. One property, and it is the headline one:
//    "cleanup runs exactly once ..." (1 of 520). A read that steals the cleanup leaves the
//    drop with nothing to do, so the handle leaks -- the direction no test notices, because
//    the process exits and the OS closes it.
//
// 4. `neon_resource_take` no longer zeroing the source slot. Failed on "the payload was
//    released exactly once ..." (6 of 505) -- the same double release as 1, reached from
//    the runtime side rather than the harness side.
//
// 5. `neon_resource_get` dropping its payload retain. Failed on "the payload was released
//    exactly once ..." (6 of 501): the caller gives back a reference it was never given.
//
// NOT CAUGHT, and analysed rather than excused: deleting the `w->release` call in
// `neon_resource_finish`, and doubling it, both leave this model green (0 of 483 and
// 0 of 539). Neither is a live defect. Every drop reaches `neon_resource_finish` through
// a `neon_resource_take` that has zeroed the payload slot, so that release is always
// `release(NULL)` and the two mutants are equivalent. Confirmed directly: with mutation 1's
// broken drop in place, deleting the release makes the model pass again (0 of 501) --
// i.e. the release in `finish` is exactly the backstop that turns a take-less drop into a
// double free, and it has no effect on any correct one. See
// `verify-resource-take-moves-the-payload-out-once`, which owns the zeroing claim.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. EXACTLY ONE CONSUMING OPERATION RUNS PER LIFE, so a bug needing e.g. `get` then
//    `cleanup` then the drop is not proved absent. This is a performance bound, and it
//    is the expensive dimension rather than a cheap one to relax: a `neon_release` whose
//    outcome CBMC cannot constant-fold leaves its object *symbolically* freed, every
//    later dereference carries that disjunction, and the drop recursion behind it
//    (release -> drop -> `neon_resource_finish` -> release -> ...) re-expands to the full
//    `--unwind` depth at each one. Measured on the model this one was split out of: one
//    extra consuming operation ahead of the final one took the run from 0.45s to over
//    300s, and cutting the choices at that position from seven to two changed nothing --
//    it is sequence DEPTH that costs, not path count.
//
//    The bound is argued rather than hoped: only `take` and `disarm` change a resource's
//    state, and every operation here is covered from both states. Everything else is a
//    pure read followed by a release, so a sequence of them differs only in the reference
//    count -- which is what the lifecycle models verify, over sequences this one does not
//    duplicate. It does mean the resource is never held at a reference count above one.
//
// 2. TAKE COUNTS ARE LITERALS AT EACH CALL SITE, not a nondet value, and that is
//    load-bearing for the same reason. Made nondeterministic, the armed and disarmed
//    states merge into one symbolic `armed` ahead of the six-way operation switch, every
//    branch of which has a drop inlined into it -- under 1s to over 5 minutes, covering
//    identical executions. Two concrete calls keep each life foldable.
//
// 3. AT MOST ONE BARE TAKE. `armed` is monotone -- once false it never returns to true --
//    so a second take is in the same state as the first failed one. That the second take
//    fails and writes nothing is proved by
//    `verify-resource-take-moves-the-payload-out-once`, not here.
//
// 4. Out-of-memory does not appear as a *return* anywhere: `neon_alloc` traps rather than
//    returning NULL, so `neon_resource_new` has no failure path to model. What
//    `--malloc-may-fail --malloc-fail-null` buys is a check that the trap terminates
//    rather than running on with a NULL header, which the `_exit` stub encodes. A leak
//    check cannot fire past a trap, so "no leak on OOM" is vacuous by design.
//
// 5. Concurrency. The refcount is a plain `uint64_t` and the runtime is single-threaded;
//    the disarm race is modelled as sequential orderings, in
//    `verify-resource-disarm-picks-exactly-one-winner`, not as simultaneous ones.
//
// 6. The closure's *body*. `cleanup.fn` is called through the pointer, as codegen does,
//    but what a user's cleanup computes is not this file's business -- only that it is
//    invoked once, with a payload it owns.
//
// 7. Payloads other than one counted pointer: larger ones, and witnesses with a `retain`
//    but no `release` or the reverse. `w->size` is still read from the witness by the
//    code under test, so the sizing arithmetic in `neon_resource_new` is exercised, just
//    at a single size.
//
// ---- Assumptions ----
//
// One, and it is an encoding assumption rather than a restriction: see ASSUMPTION 1 at
// its use. Everything else that narrows this model is a literal bound in the harness's
// control flow, visible in the code, not something CBMC is told.

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

// The payload's witness. `size` is one pointer and retain/release forward to the
// lifecycle: the shape codegen emits for a `Resource[str, E]`, whose payload carries a
// counted owner. Rule 7 -- with a scalar payload `release` is NULL and every ownership
// bug below is invisible.
static void handle_retain(void* elem) {
    neon_retain(*(neon_header**)elem);
}

static void handle_release(void* elem) {
    // The slot is zeroed by `neon_resource_take`, so on the moved-out path this is
    // `neon_release(NULL)` -- a no-op. If that zeroing ever goes away this becomes a
    // second release of a live object, and the payload's count at the end of the
    // scenario catches it.
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

// A cleanup closure borrows its environment and CONSUMES its payload. Consuming the
// payload is the case that broke: a cleanup that closes a handle and releases it is the
// normal shape, not an exotic one.
typedef void (*cleanup_fn)(neon_header* env, neon_header* payload);

static void model_cleanup(neon_header* env, neon_header* payload) {
    PROVE(env != NULL, "cleanup receives its environment");
    PROVE(payload != NULL, "cleanup receives a payload");
    cleanup_calls++;
    neon_release(payload); // consumes the payload
}

// What codegen emits as the resource's `drop`: run cleanup if still armed, then land in
// the shared tail. The `neon_resource_take` is load-bearing -- without it the payload is
// released here and again in `neon_resource_finish`.
static void model_drop(void* p) {
    neon_resource* r = (neon_resource*)p;
    neon_header* payload = NULL;
    if (neon_resource_take(r, &payload)) {
        ((cleanup_fn)r->cleanup.fn)(r->cleanup.env, payload);
    }
    neon_resource_finish(r);
}

// What the explicit Neon-level `release` compiles to: take an owned copy of the closure,
// disarm, and call it. Both natives consume a reference, so the caller retains once to
// pay for the second; the net effect is one reference consumed.
static void explicit_release(neon_resource* r) {
    neon_retain((neon_header*)r);
    neon_closure c = neon_resource_cleanup(r); // consumes one ref; c.env comes back owned
    neon_header* got = NULL;
    bool mine = neon_resource_disarm(r, &got); // consumes the other
    if (mine) {
        PROVE(got != NULL, "a winning disarm yields the payload for its caller to clean up");
        ((cleanup_fn)c.fn)(c.env, got);
    } else {
        PROVE(got == NULL, "a losing disarm leaves out untouched, so nothing is cleaned twice");
    }
    neon_release(c.env);
}

// ---- the harness ----

static neon_header* g_payload;
static neon_header* g_env;
static bool expect_armed;

// A bare `neon_resource_take`: the move-out on its own, as the drop path and any emitted
// "take the payload and clean it up here" both use it. It does NOT consume a reference,
// which is what lets it run ahead of the consuming operation without ending the life.
// Whoever wins a take owes the payload a cleanup, exactly as the emitted code does.
static void do_take(neon_resource* r) {
    neon_header* got = NULL;
    bool mine = neon_resource_take(r, &got);
    PROVE(mine == expect_armed, "take wins the cleanup if and only if the resource is armed");
    if (mine) {
        expect_armed = false;
        // Borrowing `r->cleanup.env` is safe: the resource is still alive, since a take
        // consumes no reference.
        ((cleanup_fn)r->cleanup.fn)(r->cleanup.env, got);
    }
}

// Each of these consumes exactly one reference to `r`. In this model that is always the
// last one, so each runs the emitted drop before returning -- which is the point: the
// claim is about cleanup running once whichever of them ends the life.
static void do_final_op(neon_resource* r, unsigned op) {
    if (op == 0) {
        // Take, then release the resource that no longer owns anything.
        do_take(r);
        neon_release((neon_header*)r);

    } else if (op == 1) {
        // The explicit release path: cleanup runs here rather than in the drop.
        bool was_armed = expect_armed;
        unsigned before = cleanup_calls;
        expect_armed = false;
        explicit_release(r);
        PROVE(cleanup_calls == before + (was_armed ? 1u : 0u),
              "explicit release runs cleanup if and only if it won the disarm");

    } else if (op == 2) {
        // `get`: a read that consumes the resource, so the drop runs cleanup.
        neon_header* got = NULL;
        bool live = neon_resource_get(r, &got);
        if (live) {
            neon_release(got); // the read was owned, so give it back
        }

    } else if (op == 3) {
        // The closure getter on its own.
        neon_closure c = neon_resource_cleanup(r);
        neon_release(c.env); // it was handed over retained

    } else if (op == 4) {
        (void)neon_resource_is_live(r);

    } else {
        // The bare last release: cleanup runs from the drop and nowhere else.
        neon_release((neon_header*)r);
    }
}

// One complete life of one resource: `takes` bare takes, then a nondeterministically
// chosen consuming operation that runs the drop.
//
// `takes` is a literal at every call site; see SCOPE note 2.
static void scenario(unsigned takes) {
    payload_drops = 0;
    env_drops = 0;
    cleanup_calls = 0;

    g_payload = (neon_header*)neon_alloc(0, payload_drop);
    g_env = (neon_header*)neon_alloc(0, env_drop);

    neon_closure cleanup;
    cleanup.fn = (void*)model_cleanup;
    cleanup.env = g_env; // the resource takes ownership of this reference

    // The payload's single reference moves into the resource.
    neon_resource* r = neon_resource_new(&g_payload, &handle_witness, cleanup, model_drop);

    // A reference the harness keeps for this whole scenario and releases last. It changes
    // how a double free is *detected*, and is worth stating: with it, an extra release by
    // the code under test shows up as `rc == 0` at the check below rather than as a
    // use-after-free, and a missing one as `rc == 2`. Both directions are caught, and
    // caught at the point of imbalance rather than only once something later happens to
    // touch the freed bytes.
    neon_retain(g_payload);

    PROVE(r->header.rc == 1, "a fresh resource is uniquely owned");
    PROVE(r->armed, "a fresh resource is armed, so its cleanup is still owed");
    expect_armed = true;

    // Phase one: at most one bare take. Non-consuming, so the reference count is untouched
    // and the drop cannot fire here -- that is what lets it run ahead at all.
    if (takes >= 1) {
        do_take(r);
    }

    PROVE(r->header.rc == 1, "a bare take does not touch the reference count");

    // Phase two: exactly one consuming operation, which takes the last reference and so
    // runs `model_drop` from inside itself.
    //
    // ASSUMPTION 1: `op` names one of the six operations. A pure encoding assumption --
    // it removes no behaviour, because tags above 5 name nothing. Every operation in the
    // public interface that consumes a reference is in the range.
    unsigned op = NONDET_UPTO(
        5,
        "an operation tag, not a restriction: 0-5 enumerate every public entry point "
        "that consumes a reference (take+release, explicit release, get, cleanup, "
        "is_live, bare release). Values above 5 name no operation at all.");
    do_final_op(r, op);

    // The drop has run and the resource is gone.
    PROVE(cleanup_calls == 1,
          "cleanup runs exactly once over the resource's life, from either state and "
          "whichever operation ended it");
    PROVE(env_drops == 1, "the closure environment is released exactly once");

    // The payload's references are balanced against the harness's pinning reference.
    // `rc == 0` here means it was released once too often -- the use-after-free this
    // model exists to catch; `rc == 2` means once too few, which is a leak.
    PROVE(g_payload->rc == 1,
          "the payload was released exactly once, by whoever ran the cleanup");
    PROVE(payload_drops == 0, "the payload is not dropped while the harness still holds it");

    neon_release(g_payload);
    PROVE(payload_drops == 1, "the payload is dropped exactly once, and only at rc == 0");

    // Nothing else is freed by hand. The resource and the environment must both have been
    // reclaimed by the code under test; --memory-leak-check is the assertion.
}

int main(void) {
    scenario(0); // the operation finds the resource armed
    scenario(1); // something took the payload first: it finds it disarmed
    return 0;
}
