// Model: `neon_resource_get` on a live resource, on one whose payload has already been
// taken, and from a context that does not hold the baton, with the reference count of the
// payload checked on both sides of the call.
//
// THE INVARIANT: a `neon_resource_get` that answers OK hands back an OWNED reference to
// the payload -- one the caller must release -- and does NOT disarm the body. Every other
// answer names its reason and hands back nothing: RELEASED once the payload has gone,
// NOT_OWNER from a context without the baton, `out` untouched and no retain taken on
// either. NOT_OWNER additionally disturbs nothing -- the body stays armed, still owned by
// its owner.
//
// All of it is silent when wrong. If `get` handed back a borrowed reference while the
// caller treated it as owned -- the convention every reader in this ABI follows -- the
// payload would be released once too often and the failure would land somewhere else
// entirely, on whoever still held it. If it leaked a retain, nothing would ever fail; the
// handle would simply never be closed. If `get` disarmed, a read would silently consume
// the cleanup: the resource would still look alive to its holder, but the drop would find
// it disarmed and the cleanup would never run. And if a foreign fiber's get were answered
// OK, two fibers would be interleaving reads on one descriptor -- the failure mode the
// baton exists to remove.
//
// Rule 7 is what makes the ownership half checkable at all. The payload is a COUNTED
// handle -- a `neon_header*` with a real retain/release witness -- so `get`'s retain is an
// observable increment and the caller's release an observable decrement. With a scalar
// payload the witness has no `retain` and no `release`, `neon_resource_get`'s
// `if (b->w->retain)` is simply not taken, and every claim below is vacuous. That is not
// hypothetical: every `Resource[...]` in the tree held a scalar when this code
// use-after-freed, which is exactly why the first `Resource[str, E]` found it.
//
// The `model_drop` below is what codegen emits per instantiation as the REF-drop, in the
// shape codegen emits it. It is harness, not code under test: every runtime function it
// calls is real.
//
// Verifies `src/resource.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Mutations are applied to a scratch COPY of `src/resource.c` and the model re-run
// against the copy; the shipping source is never edited. Baseline: 760 properties,
// VERIFICATION SUCCESSFUL. Five mutations, each discarded with its scratch copy.
//
// 1. The `b->w->retain(out)` deleted from `neon_resource_get`, so the read hands back a
//    borrow while every caller treats it as owned. Failed on "get hands back an OWNED
//    reference ...", on "and giving that reference back returns the count to where it
//    was ...", and on "the payload was released exactly once ...", with a
//    deallocated-object dereference at the count check (8 of 741). Shipped, every `get`
//    followed by the release codegen emits for an owned value frees the body's own
//    payload out from under it.
//
// 2. `neon_resource_get` additionally disarming -- the plausible confusion of a read with
//    a move. Failed on exactly the claims that forbid it: "get does NOT disarm ..." and
//    "cleanup ran exactly once ..." (2 of 760). Shipped, reading a resource silently
//    cancels its cleanup: the handle is never closed and nothing reports it.
//
// 3. `neon_resource_take` no longer clearing `armed`. Failed on seven, including "get
//    answers from the gate and then the armed flag ...", "get does NOT disarm ...", and
//    "get yields the payload that went in" (7 of 760).
//
// 4. `neon_resource_take` no longer zeroing the source slot. Failed on "the payload was
//    released exactly once ..." (6 of 745), so this model is a second witness to the
//    double free even though `verify-resource-take-moves-the-payload-out-once` is the one
//    that names it.
//
// 5. THE GATE DELETED -- the `owner != ctx -> NOT_OWNER` check removed from
//    `neon_resource_gate`, so a foreign fiber's read is answered OK with an owned payload
//    it was never allowed to see. Failed on "get answers from the gate and then the
//    armed flag, in that order ..." -- the foreign scenario's read comes back OK instead
//    of NOT_OWNER (1 of 760). One property, and the right one: with the gate gone the
//    foreign read is a perfectly balanced owned read, so only the claim that names WHO
//    may read can catch it.
//
// NOT CAUGHT: dropping the env retain in `neon_resource_cleanup` (0 of 760) -- this model
// never fetches the closure; that is `verify-resource-cleanup-retains-the-env-before-
// releasing`. Deleting or doubling the payload release in `neon_resource_body_drop`
// (0 of 729, 0 of 779); equivalent mutants here for the reason recorded in
// `verify-resource-take-moves-the-payload-out-once`. Dropping the gate's CLAIM arm
// (0 of 754): every body in this model is owned from birth to death, so the claim never
// fires -- it is `verify-a-transfer-leaves-exactly-one-owning-ref`'s mutation 4.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. ONE `get` PER LIFE. `neon_resource_get` writes nothing on any driven path -- it
//    reads `armed`, copies the payload out and retains it, or loses at a check -- so a
//    second get from the same state differs from the first only in the reference counts,
//    which is what the lifecycle models cover. All three *answers* are driven, which is
//    the distinction that matters, since the gate and the armed flag are everything get
//    branches on. The fourth answer -- OK via a CLAIM on an in-flight body -- needs a
//    transfer to set up and is proved in `verify-a-transfer-leaves-exactly-one-owning-
//    ref`, not here: every body here is owned from birth.
//
// 2. TWO CONSUMING OPERATIONS, and no more: the `get` and the final release. That is a
//    performance bound and sequence DEPTH is the expensive dimension -- a `neon_release`
//    CBMC cannot constant-fold leaves its object symbolically freed, every later
//    dereference carries the disjunction, and the drop recursion behind it re-expands to
//    the full `--unwind` depth at each one. One extra consuming operation took the
//    single-object predecessor of this model from 0.45s to over 300s. Both releases here
//    are foldable (rc 2 -> 1, then 1 -> 0 with a concrete drop), which is why two fit.
//    Take counts are literals at each call site for the same reason: made
//    nondeterministic, armed and disarmed merge into one symbolic flag ahead of the
//    branches -- under 1s to over 5 minutes for identical coverage.
//
// 3. THE FOREIGN SCENARIO STANDS ON FIBER-OBJECT IDENTITIES, not on the root context --
//    the tractability note at the context stub says why (the root sentinel comparison
//    against a different context does not fold in symex, and the drop recursion
//    re-expands behind the unpruned gate branch). The root-context path through
//    `neon_resource_ctx` is covered by the two same-context scenarios.
//
// 4. Which operation ends a life, over all six public entry points, is
//    `verify-resource-cleanup-runs-exactly-once`, not this model.
//
// 5. Payloads other than one counted pointer. `w->size` is read from the witness by the
//    `memcpy` in `neon_resource_get`, so the sizing is exercised, but only at one size;
//    a witness with a `release` and no `retain` (or the reverse) is not covered, and it
//    is exactly the pair of guards `neon_resource_get` and `neon_resource_body_drop`
//    branch on.
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

// ---- stubs for the modules sources.txt does not compile (rule 4's other half) ----

// resource.c routes the body's allocation through the shared heap. Under NEON_CBMC the
// allocator is plain malloc with nothing to route around; lifecycle.c already stubs
// `neon_send_routing_end` to a no-op the same way.
void* neon_shared_routing_begin(void) { return NULL; }

// The current execution context, from the fiber sources this model does not build. NULL
// is the root context (resource.c substitutes its own sentinel, so NULL never reaches an
// owner comparison); the harness points this at a static to stand on a fiber. A fiber
// here is an identity, nothing more -- the pointees are never dereferenced.
//
// TRACTABILITY: the foreign scenario stands on OBJECT identities, never on the root, and
// that is load-bearing. Symex folds `&owner_ctx_obj != &other_ctx_obj` and prunes the
// gate's dead branch; a comparison involving the root sentinel `(neon_fiber*)1` against
// any DIFFERENT context is not folded, both gate branches stay live, and the drop
// recursion re-expands behind them -- measured at under 1s against over 240s in
// `verify-resource-disarm-picks-exactly-one-winner`. Same-context sentinel comparisons
// (1 != 1) fold fine, which keeps the two root-context scenarios cheap.
static char owner_ctx_obj;
static char other_ctx_obj;
#define OWNER_FIBER ((neon_fiber*)&owner_ctx_obj)
#define OTHER_FIBER ((neon_fiber*)&other_ctx_obj)
static neon_fiber* current_fiber; // NULL: the root context
neon_fiber* neon_fiber_current(void) { return current_fiber; }

// Reached only for a closure environment flagged NEON_ALLOC_ARENA, which the CBMC
// `neon_alloc` never sets -- asserted, not assumed.
neon_header* neon_env_copy_to_shared(neon_header* env) {
    PROVE(0, "unreachable in this model: the CBMC neon_alloc never sets NEON_ALLOC_ARENA");
    return env;
}

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

// The payload's witness: rule 7. `retain` and `release` are both present and both forward
// to the lifecycle, which is what makes `neon_resource_get`'s ownership transfer an
// observable event rather than a no-op.
static void handle_retain(void* elem) {
    neon_retain(*(neon_header**)elem);
}

static void handle_release(void* elem) {
    // The slot is zeroed by `neon_resource_take`, so on the moved-out path this is
    // `neon_release(NULL)` -- a no-op.
    neon_release(*(neon_header**)elem);
}

static bool handle_eq(const void* a, const void* b) {
    return *(neon_header* const*)a == *(neon_header* const*)b;
}

// `neon_resource_new` CONSUMES the caller's payload: it deep-copies it into the body
// through `copy` and then releases the original. A counted handle's copy is
// alias-and-retain -- without it the memcpy fallback would move the pointer and the
// release would destroy the reference that just moved in.
static void handle_copy(const void* src, void* dst) {
    *(neon_header**)dst = *(neon_header* const*)src;
    neon_retain(*(neon_header**)dst);
}

static const neon_witness handle_witness = {
    .size = sizeof(neon_header*),
    .retain = handle_retain,
    .release = handle_release,
    .eq = handle_eq,
    .cmp = NULL,
    .copy = handle_copy,
};

// ---- the emitted, per-instantiation half ----

typedef void (*cleanup_fn)(neon_header* env, neon_header* payload);

static void model_cleanup(neon_header* env, neon_header* payload) {
    PROVE(env != NULL, "cleanup receives its environment");
    PROVE(payload != NULL, "cleanup receives a payload");
    cleanup_calls++;
    neon_release(payload); // consumes the payload
}

// What codegen emits as the resource's REF-drop: if this ref carries the baton and the
// body is still armed, take the payload and run cleanup, then land in the shared tail.
// Whether its take succeeds is the observable consequence of get not disarming -- after a
// get with no take, the drop must still find the body armed.
static void model_drop(void* p) {
    neon_resource* r = (neon_resource*)p;
    neon_header* payload = NULL;
    if (r->owning && neon_resource_take(r->body, &payload)) {
        ((cleanup_fn)r->body->cleanup.fn)(r->body->cleanup.env, payload);
    }
    neon_resource_ref_finish(r);
}

// ---- the harness ----

// A distinguishable non-NULL address for the `out` slot, so "does not write out" is
// checked as *untouched* rather than "still NULL". Never dereferenced.
static neon_header sentinel_obj;
#define UNWRITTEN (&sentinel_obj)

// One complete life: `takes` bare takes on `creator`'s context, then one `get` on
// `getter`'s, then the last release back on `creator`'s. `takes` and the contexts are
// literals at each call site; see SCOPE notes 2 and 3.
static void scenario(unsigned takes, neon_fiber* creator, neon_fiber* getter,
                     int64_t expect) {
    payload_drops = 0;
    env_drops = 0;
    cleanup_calls = 0;
    current_fiber = creator;

    neon_header* g_payload = (neon_header*)neon_alloc(0, payload_drop);
    neon_header* g_env = (neon_header*)neon_alloc(0, env_drop);

    neon_closure cleanup;
    cleanup.fn = (void*)model_cleanup;
    cleanup.env = g_env; // the body takes ownership of this reference

    neon_resource* r = neon_resource_new(&g_payload, &handle_witness, cleanup, model_drop);
    neon_resource_body* b = r->body;

    // A reference the harness keeps and releases last, so an imbalance in the payload is
    // caught as a count rather than as a use-after-free: `rc == 0` at the end is one
    // release too many, `rc == 2` is a leak.
    neon_retain(g_payload);
    PROVE(g_payload->rc == 2,
          "the payload's reference moved into the body; the other is the harness pin");

    // The reference `get` will consume. The harness keeps one of its own so the ref
    // survives the call and the body's state can be inspected afterwards.
    neon_retain((neon_header*)r);
    PROVE(r->header.rc == 2, "the harness holds a reference beyond the one get will take");
    PROVE(b->armed, "a fresh body is armed");
    PROVE(b->owner != NULL, "and owned by its creating context from birth");

    bool expect_armed = true;

    // A bare take, when there is one, moves the payload out of the body and consumes no
    // reference -- it is how this model reaches the state where get must answer RELEASED.
    if (takes >= 1) {
        neon_header* taken = UNWRITTEN;
        if (neon_resource_take(b, &taken)) {
            expect_armed = false;
            // We own the payload now and owe it a cleanup, exactly as emitted code does.
            ((cleanup_fn)b->cleanup.fn)(b->cleanup.env, taken);
        }
    }

    // ---- the get ----

    current_fiber = getter;
    uint64_t payload_rc_before = g_payload->rc;
    neon_header* got = UNWRITTEN;
    int64_t code = neon_resource_get(r, &got);

    PROVE(code == expect,
          "get answers from the gate and then the armed flag, in that order: OK exactly "
          "while the caller holds the baton and the payload is still the body's to give, "
          "NOT_OWNER from a foreign context, RELEASED once the payload has gone");
    PROVE(r->header.rc == 1, "get consumes one reference to the ref, whatever it answers");
    PROVE(b->armed == expect_armed,
          "get does NOT disarm: a read never claims the cleanup out from under the drop");

    if (code == NEON_RESOURCE_OK) {
        PROVE(got == g_payload, "get yields the payload that went in");
        PROVE(g_payload->rc == payload_rc_before + 1,
              "get hands back an OWNED reference: it retains the payload through the "
              "witness before returning it, so the caller owes exactly one release");
        PROVE(*(neon_header**)neon_resource_body_payload(b) == g_payload,
              "and leaves the body's own copy in place -- get reads, it does not move");
        neon_release(got); // the caller's owed release
        PROVE(g_payload->rc == payload_rc_before,
              "and giving that reference back returns the count to where it was: get is "
              "balanced against one release, not a leak and not a borrow");
    } else {
        PROVE(got == UNWRITTEN,
              "a losing get does not write out at all, so a use-after-release or a "
              "foreign read is a diagnosable code rather than a stale handle");
        PROVE(g_payload->rc == payload_rc_before,
              "and retains nothing when it reports a loss");
        if (code == NEON_RESOURCE_NOT_OWNER) {
            PROVE(b->armed,
                  "a foreign get loses at the gate with NOT_OWNER, before the armed "
                  "check -- and disturbs nothing: the body is still armed for its owner");
        }
    }

    // ---- the last release: the ref-drop runs, and finds whatever get left behind ----

    current_fiber = creator;
    neon_release((neon_header*)r);

    PROVE(cleanup_calls == 1,
          "cleanup ran exactly once: the drop still found the body armed when only a get "
          "had happened, and found it disarmed when a take had already won");
    PROVE(env_drops == 1,
          "the closure environment is released exactly once, by the body's own drop");
    PROVE(g_payload->rc == 1,
          "the payload was released exactly once, by whoever ran the cleanup -- get's "
          "owned reference was given back and released nothing extra");
    PROVE(payload_drops == 0, "the payload is not dropped while the harness still holds it");

    neon_release(g_payload);
    PROVE(payload_drops == 1, "the payload is dropped exactly once, and only at rc == 0");

    // Nothing else is freed by hand. The ref, the body and the environment must all have
    // been reclaimed by the code under test; --memory-leak-check is the assertion.
}

int main(void) {
    // get on a live resource, on the owning root context: an owned payload, and the body
    // still armed.
    scenario(0, NULL, NULL, NEON_RESOURCE_OK);
    // a take moved the payload out first: get must answer RELEASED and write nothing.
    scenario(1, NULL, NULL, NEON_RESOURCE_RELEASED);
    // a foreign fiber reads a resource owned by another fiber: NOT_OWNER, nothing
    // written, nothing retained, nothing disturbed.
    scenario(0, OWNER_FIBER, OTHER_FIBER, NEON_RESOURCE_NOT_OWNER);
    return 0;
}
