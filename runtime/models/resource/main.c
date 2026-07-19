// Model: resources -- arm/disarm, move-out, and the exactly-once cleanup.
//
// Drives `neon_resource_new` / `_take` / `_disarm` / `_get` / `_cleanup` / `_is_live` /
// `_finish` from `src/resource.c` -- the shipping source, compiled by CBMC alongside this
// harness, never a copy -- through every ordering of the operations up to a bound.
//
// ---- The payload is counted, on purpose ----
//
// This code use-after-freed earlier today: an emitted drop moved the payload out and
// handed it to a cleanup that consumes it, and then `neon_resource_finish` released the
// same bytes a second time. Nothing caught it, because every `Resource[...]` in the tree
// held a scalar, and a scalar's witness has no `release` -- so the second release was a
// call through a NULL function pointer that never happened. The first `Resource[str, E]`
// found it.
//
// So the payload here is a *counted handle*: a `neon_header*` whose witness retains and
// releases it. Any double-release of the payload slot is therefore a real second
// `neon_release` on a real object, which CBMC sees as a dereference of a deallocated
// object (it is `free`d at rc == 0) or as a drop count of 2. A model with a scalar payload
// would pass and prove nothing; that is the whole point of this file.
//
// ---- What is proved ----
//
// Over every sequence of up to two operations on a live resource, followed by every
// choice of which operation performs the final release and so runs the emitted drop:
//
//   - cleanup runs EXACTLY ONCE, on every ordering of take / disarm / get / cleanup /
//     is_live / release and the drop, whichever of them wins the disarm;
//   - the payload object is dropped exactly once -- no double free, no leak, and no
//     use-after-free from `neon_resource_finish` releasing bytes whose ownership moved;
//   - `neon_resource_take` on an already-disarmed resource returns false and does not
//     write `out`, so the second caller does not get the payload;
//   - the same for `neon_resource_disarm`, which is the disarm-first safety property:
//     of every caller, exactly one is told it owns the cleanup;
//   - after a take, the payload slot is zeroed, so the release in `neon_resource_finish`
//     cannot reach bytes whose ownership has already moved;
//   - the closure environment is retained by `neon_resource_cleanup` before the resource
//     is released -- otherwise `explicit_release` below is a use-after-free -- and is
//     released exactly once overall;
//   - `neon_resource_get` hands back an *owned* payload and does not disarm;
//   - `neon_resource_is_live` agrees with the harness's own tracking of the flag;
//   - the resource itself is freed exactly once (--memory-leak-check);
//   - `neon_resource_finish`'s resurrection assertion holds on every path.
//
// The `model_drop` / `explicit_release` pair below is what codegen emits per
// instantiation, in the shape codegen emits it. It is harness, not code under test: every
// runtime function it calls is the real one. Substituting the buggy drop -- delete the
// `neon_resource_take` and read the payload slot directly -- makes this model fail with a
// double free, which is the check that it has teeth.
//
// ---- Shape of the harness, and why it is shaped that way ----
//
// Two phases. Phase one runs a sequence of operations with an extra reference held
// across each, so none of them can be the last release. Phase two picks *which*
// operation performs the last release, and so runs the drop from inside that operation.
//
// This is not a restriction: every native releases the resource as its final act, so a
// drop firing inside one is the same execution as a drop firing immediately after it
// returns, and phase two covers all six choices of which one that is. The shape exists
// because it keeps the reference count *concrete* on every path -- CBMC then folds away
// the "is this the last release" branch at the operations that provably are not, instead
// of inlining the whole drop, `neon_resource_finish`, and their nested releases at every
// one of them. With a symbolic count this same model does not finish in an hour; with a
// concrete one it is seconds. The behaviours covered are the same.
//
// ---- What is deliberately NOT proved ----
//
//   - Sequences longer than three operations. See ASSUMPTION 1.
//   - Out-of-memory does not appear as a *return* anywhere. `neon_alloc` traps rather
//     than returning NULL, so `neon_resource_new` has no failure path to model. What
//     `--malloc-may-fail --malloc-fail-null` buys here is a check that the trap
//     terminates rather than running on with a NULL header, which the `_exit` stub in
//     cbmc_support.h encodes.
//   - Concurrency. The refcount is a plain `uint64_t` and the runtime is single-threaded;
//     "ordering" above means sequential orderings, not simultaneous ones.
//   - Refcount overflow at 2^64. Unreachable in any finite execution.
//   - The closure's *body*. `cleanup.fn` is called through the pointer, as codegen does,
//     but what a user's cleanup computes is not this file's business -- only that it is
//     invoked once, with a payload it owns.
//   - Payloads larger than one pointer, and payloads with a `retain` but no `release` (or
//     the reverse). The witness here is the counted-handle shape, which is the one that
//     broke; `w->size` is still read from the witness by the code under test, so the
//     sizing arithmetic in `neon_resource_new` is exercised, just at one size.
//
// ---- Assumptions ----
//
// Two, both bounding, neither excluding a behaviour of the code under test. Each is
// justified at its use, and ASSUMPTION 1 is the only one that costs coverage.

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

// The payload's witness. `size` is one pointer and retain/release forward to the
// lifecycle: the shape codegen emits for a `Resource[str, E]`, whose payload carries a
// counted owner.
static void handle_retain(void* elem) {
    neon_retain(*(neon_header**)elem);
}
static void handle_release(void* elem) {
    // The slot is zeroed by `neon_resource_take`, so on the moved-out path this is
    // `neon_release(NULL)` -- a no-op. If that zeroing ever goes away this becomes a
    // second release of a live object, and the drop counts below catch it.
    neon_release(*(neon_header**)elem);
}
static bool handle_eq(const void* a, const void* b) {
    return *(neon_header* const*)a == *(neon_header* const*)b;
}

static const neon_witness handle_witness = {
    sizeof(neon_header*), handle_retain, handle_release, handle_eq, NULL,
};

// ---- the emitted, per-instantiation half ----

// A cleanup closure borrows its environment and CONSUMES its payload. Consuming the
// payload is the case that broke: a cleanup that closes a handle and releases it is the
// normal shape, not an exotic one.
typedef void (*cleanup_fn)(neon_header* env, neon_header* payload);

static void model_cleanup(neon_header* env, neon_header* payload) {
    PROVE(env != NULL, "cleanup receives its environment");
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
        PROVE(payload != NULL, "a successful take in the drop yields the payload");
        ((cleanup_fn)r->cleanup.fn)(r->cleanup.env, payload);
    }
    neon_resource_finish(r);
}

// What the explicit Neon-level `release` compiles to: take an owned copy of the closure,
// disarm, and call it. Both natives consume a reference, so the caller retains once to
// pay for the second; the net effect is one reference consumed.
//
// This is the shape that catches a missing retain in `neon_resource_cleanup`: were the
// environment handed back unretained, the `neon_resource_disarm` below could be the last
// release and `c.env` would be dangling by the time it is called.
static void explicit_release(neon_resource* r) {
    neon_retain((neon_header*)r);
    neon_closure c = neon_resource_cleanup(r); // consumes one ref; c.env comes back owned
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

static neon_header* g_payload;
static neon_header* g_env;
static bool expect_armed;

// Every operation below consumes exactly one reference to `r`, so the caller controls
// whether it is the last one.
static void do_op(neon_resource* r, unsigned op) {
    if (op == 0) {
        // Bare `take` -- the move-out on its own, as the drop path and any emitted
        // "take the payload and clean it up here" both use it. Non-consuming, so a
        // plain release pays for the reference.
        neon_header* got = NULL;
        bool mine = neon_resource_take(r, &got);
        PROVE(mine == expect_armed, "take succeeds if and only if the resource is armed");
        if (mine) {
            PROVE(got == g_payload, "take yields the payload that went in");
            PROVE(!r->armed, "take disarms the resource");
            PROVE(*(neon_header**)neon_resource_payload(r) == NULL,
                  "take zeroes the payload slot at the source");
            expect_armed = false;
            // We now own the payload and so owe it a cleanup, exactly as the emitted
            // code does. Borrowing `r->cleanup.env` is safe: we still hold a reference.
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
              "explicit release runs cleanup if and only if it won the disarm");

    } else if (op == 2) {
        // `get`: an owned read that must NOT disarm.
        neon_header* got = NULL;
        bool live = neon_resource_get(r, &got);
        PROVE(live == expect_armed, "get reports liveness");
        if (live) {
            PROVE(got == g_payload, "get yields the payload");
            neon_release(got); // the read was owned, so give it back
        } else {
            PROVE(got == NULL, "get on a released resource leaves out untouched");
        }
        // `expect_armed` is unchanged: get must not disarm. Were it ever to, the
        // liveness assertions on the following operation would fire.

    } else if (op == 3) {
        // The closure getter on its own.
        neon_closure c = neon_resource_cleanup(r);
        PROVE(c.env == g_env, "the closure comes back intact");
        neon_release(c.env); // it was handed over retained

    } else if (op == 4) {
        bool live = neon_resource_is_live(r);
        PROVE(live == expect_armed, "is_live agrees with the armed flag");

    } else {
        neon_release((neon_header*)r);
    }
}

int main(void) {
    g_payload = (neon_header*)neon_alloc(0, payload_drop);
    g_env = (neon_header*)neon_alloc(0, env_drop);

    neon_closure cleanup;
    cleanup.fn = (void*)model_cleanup;
    cleanup.env = g_env; // the resource takes ownership of this reference

    // The payload's single reference moves into the resource.
    neon_resource* r =
        neon_resource_new(&g_payload, &handle_witness, cleanup, model_drop);

    PROVE(r->header.rc == 1, "a fresh resource has rc == 1");
    PROVE(r->armed, "a fresh resource is armed");
    PROVE(*(neon_header**)neon_resource_payload(r) == g_payload,
          "the payload is copied into the inline slot");
    expect_armed = true;

    // ASSUMPTION 1: at most two operations before the final one, so at most three in
    // total. This is the only assumption that costs coverage, and it costs the least
    // that any bound can: the reachable state of a resource is just `armed`, which is
    // monotone -- once false it never returns to true -- so an operation sequence has
    // at most one transition in it. Three operations reach every arrangement around
    // that transition: before it, the operation that causes it, and after it. A fourth
    // adds a second operation on one side of a transition that has already been
    // observed from that side. What is genuinely not covered is a bug that needs *two*
    // operations of a specific kind on the same side, e.g. a counter that goes wrong
    // only on the third `get`; there is no such state in this code, but the model
    // cannot prove that, which is why it is written down here.
    unsigned k = NONDET_UPTO(
        2,
        "sequence length; `armed` is monotone so three operations reach every "
        "arrangement around its single transition");

    for (unsigned i = 0; i < k; i++) {
        // ASSUMPTION 2: `op` names one of the six operations. A pure encoding
        // assumption -- it removes no behaviour, because tags above 5 name nothing.
        unsigned op = NONDET_UPTO(
            5,
            "an operation tag, not a restriction: 0-5 enumerate every public entry "
            "point that consumes a reference (take+release, explicit release, get, "
            "cleanup, is_live, bare release). Values above 5 name no operation.");
        // Hold an extra reference across the operation so it cannot be the last
        // release. The final release is phase two, below.
        neon_retain((neon_header*)r);
        do_op(r, op);
    }

    PROVE(r->header.rc == 1, "the resource is still held by exactly one reference");

    // Phase two: the last reference goes, inside whichever operation is chosen. This is
    // where `model_drop` runs.
    unsigned last = NONDET_UPTO(
        5,
        "which operation performs the final release and so runs the emitted drop; the "
        "same six tags as above");
    do_op(r, last);

    // Everything is settled: the drop has run.
    PROVE(cleanup_calls == 1, "cleanup runs exactly once, on every ordering");
    PROVE(payload_drops == 1, "the payload is dropped exactly once");
    PROVE(env_drops == 1, "the closure environment is released exactly once");

    // Nothing is freed here. The resource, the payload and the environment must all have
    // been reclaimed by the code under test; --memory-leak-check is the assertion.
    return 0;
}
