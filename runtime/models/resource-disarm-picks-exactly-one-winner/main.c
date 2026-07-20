// Model: two callers racing to disarm one resource, from both the armed state and the
// state a take has already claimed.
//
// THE INVARIANT: of every caller that races to disarm a resource, EXACTLY ONE is told it
// owns the cleanup. The losers get `false` with `out` untouched.
//
// This is the disarm-then-act safety property, and it is the reason
// `neon_resource_disarm` disarms *first* and releases second rather than the other way
// round. Two callers can legitimately hold references to the same resource and both decide
// to release it -- an explicit Neon-level `release` on one path and a last-reference drop
// on another is the ordinary case, not a contrived one -- and the two paths know nothing
// about each other. If both were told they own the cleanup, the second `close` lands on a
// descriptor the OS has already handed to someone else: no crash, no error, and the damage
// is in an unrelated part of the program. If neither were, the handle leaks silently.
//
// `out` being untouched on the losing path is half the property rather than a detail. A
// loser that gets `false` but has the payload written into `out` anyway is one careless
// caller away from cleaning up an object it does not own, and the emitted code's shape --
// `if (disarm(r, &p)) cleanup(p)` -- makes that a live risk rather than a theoretical one.
// This model checks it as *untouched*, against a sentinel, not merely "still NULL".
//
// Rule 7: the payload is a COUNTED handle -- a `neon_header*` with a real retain/release
// witness -- never a scalar. Two winners then means a real second `neon_release` on a real
// object, caught as an unbalanced reference count. Every `Resource[...]` in the tree held
// a scalar when this code use-after-freed, and a scalar's witness has no `release`, so the
// second release was a call through a NULL function pointer that never happened.
//
// Verifies `src/resource.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Baseline: 423 properties, VERIFICATION SUCCESSFUL. Two mutations, both reverted.
//
// 1. `neon_resource_take` no longer clearing `armed`, so `neon_resource_disarm` never
//    actually disarms and both racers are told they won. Failed on "exactly one caller is
//    told it owns the cleanup -- never two, which would close a reused descriptor, and
//    never zero, which would leak the handle", on "and so the cleanup ran exactly once",
//    and on "a winning disarm yields the payload that went in" (4 of 417). This is the
//    mutation the model exists for, and the cost is in the claim text: two `close`
//    syscalls on one descriptor, the second landing on whatever the OS handed out in
//    between.
//
// 2. `neon_resource_take` no longer zeroing the source slot. Failed on "the payload was
//    released exactly once, by the single caller that won" with a deallocated-object
//    dereference at the count check (6 of 408) -- the winner cleans up the payload and
//    `neon_resource_finish` then releases the same bytes out of the slot the winner
//    already emptied.
//
// NOT CAUGHT, and correctly so -- these are other models' claims, listed so the boundary
// is on record: dropping the env retain in `neon_resource_cleanup` (0 of 423, owned by
// `verify-resource-cleanup-retains-the-env-before-releasing`), dropping the payload retain
// in `neon_resource_get` and making `get` disarm (0 of 423 each, owned by
// `verify-resource-get-hands-back-an-owned-payload`). This model runs neither `get` nor
// the closure getter.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. TWO RACERS, NOT N. The claim generalises by the same argument the code does: `armed`
//    is monotone and `neon_resource_take` is its only writer, so caller k+1 is in exactly
//    the state caller 2 is in here -- the loser's state. Two is the smallest number that
//    distinguishes "exactly one" from "at least one", which is what makes it enough.
//
// 2. ONE ORDER OF THE TWO RACERS. They are the same call with the same arguments, so
//    swapping them relabels the transcript and nothing else. What is genuinely covered
//    from both sides is the *state* they race in: SCENARIO 0 is two disarms on an armed
//    resource, SCENARIO 1 is two disarms after a take has already won.
//
// 3. CONCURRENCY, in the strict sense. The refcount is a plain `uint64_t`, `armed` is a
//    plain `bool`, and the runtime is single-threaded; "race" here means the two
//    sequential orderings a program can produce, not simultaneous execution. A
//    genuinely concurrent disarm would need an atomic `armed` and is not what this code
//    claims.
//
// 4. EXACTLY ONE OF THE TWO DISARMS RUNS THE DROP -- the second, which takes the last
//    reference. The first release is foldable (rc 2 -> 1, no drop), which is why two
//    consuming operations are affordable here: a `neon_release` CBMC *cannot*
//    constant-fold leaves its object symbolically freed and re-expands the drop recursion
//    at every later dereference, which took the model this was split out of from 0.45s to
//    over 300s. Take counts are literals at each call site for the same reason -- made
//    nondeterministic, armed and disarmed merge into one symbolic flag ahead of the
//    branches, under 1s to over 5 minutes for identical coverage.
//
// 5. Which operation runs cleanup across the whole public interface is proved by
//    `verify-resource-cleanup-runs-exactly-once`, not here. This model asserts
//    exactly-once only over the take/disarm/disarm sequence it drives.
//
// 6. Out-of-memory does not appear as a *return*: `neon_alloc` traps rather than returning
//    NULL. `--malloc-may-fail --malloc-fail-null` buys a check that the trap terminates
//    rather than running on with a NULL header, which the `_exit` stub encodes; a leak
//    check cannot fire past a trap.

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

typedef void (*cleanup_fn)(neon_header* env, neon_header* payload);

static void model_cleanup(neon_header* env, neon_header* payload) {
    PROVE(env != NULL, "cleanup receives its environment");
    PROVE(payload != NULL, "cleanup receives a payload");
    cleanup_calls++;
    neon_release(payload); // consumes the payload
}

// What codegen emits as the resource's `drop`: run cleanup if still armed, then land in
// the shared tail. Reached on the second racer's release.
static void model_drop(void* p) {
    neon_resource* r = (neon_resource*)p;
    neon_header* payload = NULL;
    if (neon_resource_take(r, &payload)) {
        ((cleanup_fn)r->cleanup.fn)(r->cleanup.env, payload);
    }
    neon_resource_finish(r);
}

// ---- the harness ----

// A distinguishable non-NULL address for the `out` slot. Never dereferenced: it exists so
// that "does not write out" is checked as *untouched*, which is stronger than "still
// NULL". Emitted callers initialise `out` to NULL, so untouched reads as NULL there.
static neon_header sentinel_obj;
#define UNWRITTEN (&sentinel_obj)

static neon_header* g_payload;
static unsigned owners; // how many callers have been told they own the cleanup

// One racer. It disarms and, if it won, immediately runs the cleanup with the payload it
// was handed -- the shape emitted code has. Borrowing `r->cleanup.env` before the other
// racer's release is what makes that safe here; the explicit release path, which must
// take an *owned* closure instead, is
// `verify-resource-cleanup-retains-the-env-before-releasing`.
//
// `neon_resource_disarm` consumes a reference, so each racer pays for one.
static void racer(neon_resource* r) {
    neon_header* got = UNWRITTEN;
    // The environment must be read before the disarm: the disarm may take the last
    // reference and free the resource under us.
    neon_header* env = r->cleanup.env;
    void* fn = r->cleanup.fn;

    bool mine = neon_resource_disarm(r, &got);
    if (mine) {
        owners++;
        PROVE(got == g_payload, "a winning disarm yields the payload that went in");
        ((cleanup_fn)fn)(env, got);
    } else {
        PROVE(got == UNWRITTEN,
              "a losing disarm does not write out at all, so the loser is never handed a "
              "payload it does not own");
    }
}

// One complete life: `takes` bare takes (non-consuming, so they cannot end it), then two
// callers each racing to disarm. `takes` is a literal at each call site; see SCOPE note 4.
static void scenario(unsigned takes) {
    payload_drops = 0;
    env_drops = 0;
    cleanup_calls = 0;
    owners = 0;

    g_payload = (neon_header*)neon_alloc(0, payload_drop);
    neon_header* g_env = (neon_header*)neon_alloc(0, env_drop);

    neon_closure cleanup;
    cleanup.fn = (void*)model_cleanup;
    cleanup.env = g_env; // the resource takes ownership of this reference

    neon_resource* r = neon_resource_new(&g_payload, &handle_witness, cleanup, model_drop);

    // A reference the harness keeps and releases last, so an imbalance is caught as a
    // count rather than as a use-after-free: `rc == 0` at the end means two winners both
    // cleaned up, `rc == 2` means neither did.
    neon_retain(g_payload);

    // The second racer's reference. Both disarms consume one, and the second takes the
    // last -- which is what runs the emitted drop.
    neon_retain((neon_header*)r);
    PROVE(r->header.rc == 2, "two callers hold the resource, and each will disarm it");
    PROVE(r->armed, "a fresh resource is armed, so its cleanup is still unclaimed");

    // A take, when there is one, is a third contender for the same cleanup -- and the
    // one that wins it, since it runs first. It consumes no reference.
    if (takes >= 1) {
        neon_header* got = UNWRITTEN;
        if (neon_resource_take(r, &got)) {
            owners++;
            PROVE(got == g_payload, "a winning take yields the payload that went in");
            ((cleanup_fn)r->cleanup.fn)(r->cleanup.env, got);
        }
        PROVE(r->header.rc == 2, "a take consumes no reference to the resource");
    }

    racer(r); // consumes one reference
    racer(r); // consumes the last, so the emitted drop runs from inside it

    PROVE(owners == 1,
          "exactly one caller is told it owns the cleanup -- never two, which would close "
          "a reused descriptor, and never zero, which would leak the handle");
    PROVE(cleanup_calls == 1, "and so the cleanup ran exactly once");
    PROVE(env_drops == 1, "the closure environment is released exactly once");

    PROVE(g_payload->rc == 1,
          "the payload was released exactly once, by the single caller that won");
    PROVE(payload_drops == 0, "the payload is not dropped while the harness still holds it");

    neon_release(g_payload);
    PROVE(payload_drops == 1, "the payload is dropped exactly once, and only at rc == 0");

    // Nothing else is freed by hand. The resource and the environment must both have been
    // reclaimed by the code under test; --memory-leak-check is the assertion.
}

int main(void) {
    scenario(0); // two racers on an armed resource: one of them wins
    scenario(1); // a take won first: both racers must lose
    return 0;
}
