// Model: callers racing to disarm one resource, from the armed state, from the state a
// take has already claimed, and -- new with the baton -- from the wrong side of the owner
// gate, in both orders.
//
// THE INVARIANT: of every caller that races to disarm a resource, EXACTLY ONE is told it
// owns the cleanup (NEON_RESOURCE_OK). Every loser gets a code naming why -- RELEASED
// when the cleanup was already claimed, NOT_OWNER when the caller's context does not hold
// the baton -- with `out` untouched, and a NOT_OWNER loser disturbs nothing: the body
// stays armed for the true owner.
//
// This is the disarm-then-act safety property, and the disarm race is now ALSO the
// owner-gated race: `neon_resource_disarm` passes the gate (is this context allowed to
// touch the body?) before it passes the armed check (is the cleanup still unclaimed?),
// in that order, and both answers must lose safely. Two callers can legitimately hold
// references and both decide to release -- an explicit Neon-level `release` on one path
// and a last-reference drop on another is the ordinary case -- and a fiber that was
// merely shown a resource (a non-owning alias, or another fiber entirely) can try too.
// If two are told they own the cleanup, the second `close` lands on a descriptor the OS
// already reused: no crash, no error, damage in an unrelated part of the program. If
// none are, the handle leaks silently. And if a NOT_OWNER loser could disarm on its way
// out, the gate would be a report, not a gate.
//
// `out` being untouched on every losing path is half the property rather than a detail. A
// loser told RELEASED or NOT_OWNER but with the payload written into `out` anyway is one
// careless caller from cleaning up an object it does not own, and the emitted code's
// shape -- `if (disarm(r, &p) == OK) cleanup(p)` -- makes that a live risk rather than a
// theoretical one. This model checks it as *untouched*, against a sentinel, not merely
// "still NULL".
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
// Mutations are applied to a scratch COPY of `src/resource.c` and the model re-run
// against the copy; the shipping source is never edited. Baseline: 655 properties,
// VERIFICATION SUCCESSFUL. Three mutations, each discarded with its scratch copy.
//
// 1. `neon_resource_take` no longer clearing `armed`, so `neon_resource_disarm` never
//    actually disarms and every gate-passing racer is told it won. Failed on "exactly one
//    caller is told it owns the cleanup", "and so the cleanup ran exactly once", "each
//    ordering produces the code it claims", and the payload count checks (5 of 655).
//    This is the mutation the model exists for, and the cost is in the claim text: two
//    `close` syscalls on one descriptor, the second landing on whatever the OS handed
//    out in between.
//
// 2. `neon_resource_take` no longer zeroing the source slot. Failed on "the payload was
//    released exactly once, by the single caller that won" with deallocated-object
//    dereferences at the count checks (6 of 640) -- the winner cleans up the payload
//    and the body's own drop then releases the same bytes out of the slot the winner
//    already emptied.
//
// 3. THE GATE DELETED -- the `owner != ctx -> NOT_OWNER` check removed from
//    `neon_resource_gate`, so a foreign fiber's disarm walks straight to the armed check
//    and wins. Failed on "each ordering produces the code it claims, and no other" (the
//    foreign racer is answered OK or RELEASED instead of NOT_OWNER, in both orderings)
//    and on "a NOT_OWNER loser disturbs nothing: the body is still armed for its owner"
//    (2 of 655). Shipped, this is two fibers interleaving closes on one descriptor --
//    the silent failure mode the baton exists to remove.
//
// NOT CAUGHT, and correctly so -- these are other models' claims, listed so the boundary
// is on record: dropping the env retain in `neon_resource_cleanup` (0 of 655, owned by
// `verify-resource-cleanup-retains-the-env-before-releasing`); dropping the payload
// retain in `neon_resource_get` and making `get` disarm (0 of 655 each, owned by
// `verify-resource-get-hands-back-an-owned-payload`; this model runs neither `get` nor
// the closure getter). Dropping the CLAIM arm of the gate (`owner == NULL && owning`) is
// also invisible here (0 of 649): every body in this model is owned from birth to death,
// so the claim never fires -- the claim is `verify-a-transfer-leaves-exactly-one-owning-
// ref`'s ground, where it is mutation 4.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. TWO RACERS PER LIFE, NOT N. The claim generalises by the same argument the code
//    does: `armed` is monotone with `neon_resource_take` its only writer, and the gate
//    reads `owner`, which no losing path writes -- so racer k+1 is in exactly the state
//    racer 2 is in here, one of the two losing states. Two is the smallest number that
//    distinguishes "exactly one" from "at least one", which is what makes it enough.
//
// 2. THE ORDERINGS ARE THE FOUR ENUMERATED IN MAIN, each concrete: two same-context
//    racers on an armed body, two after a take, and the foreign racer before and after
//    the owner's. Two same-context racers are the same call with the same arguments, so
//    swapping them relabels the transcript; the foreign racer is NOT symmetric with the
//    owner's -- the gate answers by context -- which is exactly why both of its positions
//    are driven. Expected codes are literals per ordering: this model claims WHICH loss
//    each ordering produces, not merely that losses happen.
//
// 3. CONCURRENCY, in the strict sense. `armed` and `owner` are plain fields --
//    neon/resource.h's handoff argument for why that is sound -- and "race" here means
//    the sequential orderings a program can produce, not simultaneous execution. A
//    genuinely concurrent disarm would need an atomic `armed` and is not what this code
//    claims.
//
// 4. THE RACERS SHARE ONE REF, retained once per extra racer, rather than each holding a
//    wcopy-minted ref of its own. The gate reads `r->owning` only on the claim arm, which
//    never fires here (see NOT CAUGHT); on every driven path the gate consults `owner`
//    against the calling context, so a non-owning alias from the same fiber would walk
//    the same branches. Aliases as distinct ref objects are the transfer model's ground.
//    Consuming-release costs also bound this: each racer's disarm consumes a reference,
//    the last one runs the drop, and a `neon_release` CBMC cannot constant-fold
//    re-expands the drop recursion at every later dereference -- 0.45s to over 300s for
//    one extra unfoldable consuming operation, measured on this model's single-object
//    predecessor. Take counts are literals at each call site for the same reason.
//
// 5. Which operation runs cleanup across the whole public interface is proved by
//    `verify-resource-cleanup-runs-exactly-once`, not here. This model asserts
//    exactly-once only over the take/disarm/disarm sequences it drives.
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
// TRACTABILITY: the two-fiber scenarios stand on OBJECT identities, never on the root,
// and that is load-bearing. CBMC's symex folds `&owner_ctx_obj != &other_ctx_obj` to a
// constant and prunes the gate's dead branch; a comparison involving the root sentinel
// `(neon_fiber*)1` against any DIFFERENT context (object address or other integer
// address) is not folded, both gate branches stay live, `armed` and the payload slot go
// symbolic behind them, and the drop recursion re-expands to the full `--unwind` depth
// at every later release -- measured at 0.75s against over 240s on scenario 3 alone.
// Same-context sentinel comparisons (1 != 1) fold fine, which is what keeps the two
// root-context scenarios cheap and the root path covered.
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

// `neon_resource_new` CONSUMES the caller's payload: it deep-copies it into the body
// through `copy` and then releases the original. A counted handle's copy is
// alias-and-retain.
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
// Reached on the last racer's release.
static void model_drop(void* p) {
    neon_resource* r = (neon_resource*)p;
    neon_header* payload = NULL;
    if (r->owning && neon_resource_take(r->body, &payload)) {
        ((cleanup_fn)r->body->cleanup.fn)(r->body->cleanup.env, payload);
    }
    neon_resource_ref_finish(r);
}

// ---- the harness ----

// A distinguishable non-NULL address for the `out` slot. Never dereferenced: it exists so
// that "does not write out" is checked as *untouched*, which is stronger than "still
// NULL". Emitted callers initialise `out` to NULL, so untouched reads as NULL there.
static neon_header sentinel_obj;
#define UNWRITTEN (&sentinel_obj)

static neon_header* g_payload;
static unsigned owners; // how many callers have been told they own the cleanup

// One racer, standing on `ctx`. It disarms and, if it won, immediately runs the cleanup
// with the payload it was handed -- the shape emitted code has. `expect` is the code this
// ordering must produce; the generic exactly-one claim is `owners == 1` at the end.
// Borrowing `r->body->cleanup.env` before the other racer's release is what makes the
// winner's call safe here; the explicit release path, which must take an *owned* closure
// instead, is `verify-resource-cleanup-retains-the-env-before-releasing`.
//
// `neon_resource_disarm` consumes a reference, so each racer pays for one.
static void racer(neon_resource* r, neon_fiber* ctx, int64_t expect) {
    current_fiber = ctx;
    neon_header* got = UNWRITTEN;
    // The closure must be read before the disarm: the disarm may take the last reference
    // and free the ref (and with it the body) under us.
    neon_header* env = r->body->cleanup.env;
    void* fn = r->body->cleanup.fn;

    int64_t code = neon_resource_disarm(r, &got);
    PROVE(code == expect, "each ordering produces the code it claims, and no other");
    if (code == NEON_RESOURCE_OK) {
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
// callers racing to disarm, on the contexts and in the order the caller enumerates. The
// body is born on `creator`, which therefore owns it. `takes` and the contexts are
// literals at each call site; see SCOPE notes 2 and 4.
static void scenario(unsigned takes, neon_fiber* creator, neon_fiber* ctx1, int64_t expect1,
                     neon_fiber* ctx2, int64_t expect2) {
    payload_drops = 0;
    env_drops = 0;
    cleanup_calls = 0;
    owners = 0;
    current_fiber = creator;

    g_payload = (neon_header*)neon_alloc(0, payload_drop);
    neon_header* g_env = (neon_header*)neon_alloc(0, env_drop);

    neon_closure cleanup;
    cleanup.fn = (void*)model_cleanup;
    cleanup.env = g_env; // the body takes ownership of this reference

    neon_resource* r = neon_resource_new(&g_payload, &handle_witness, cleanup, model_drop);
    neon_resource_body* b = r->body;

    // A reference the harness keeps and releases last, so an imbalance is caught as a
    // count rather than as a use-after-free: `rc == 0` at the end means two winners both
    // cleaned up, `rc == 2` means none did.
    neon_retain(g_payload);

    // The second racer's reference. Both disarms consume one, and the second takes the
    // last -- which is what runs the emitted ref-drop.
    neon_retain((neon_header*)r);
    PROVE(r->header.rc == 2, "two callers hold the resource, and each will disarm it");
    PROVE(b->armed, "a fresh body is armed, so its cleanup is still unclaimed");
    PROVE(b->owner != NULL, "and owned by its creating context from birth -- never in flight");

    // A take, when there is one, is a third contender for the same cleanup -- and the
    // one that wins it, since it runs first. It consumes no reference.
    if (takes >= 1) {
        neon_header* got = UNWRITTEN;
        if (neon_resource_take(b, &got)) {
            owners++;
            PROVE(got == g_payload, "a winning take yields the payload that went in");
            ((cleanup_fn)b->cleanup.fn)(b->cleanup.env, got);
        }
        PROVE(r->header.rc == 2, "a take consumes no reference to the ref");
    }

    bool armed_before = b->armed;
    racer(r, ctx1, expect1); // consumes one reference
    if (expect1 == NEON_RESOURCE_NOT_OWNER) {
        PROVE(b->armed == armed_before,
              "a NOT_OWNER loser disturbs nothing: the body is still armed for its owner");
    }
    racer(r, ctx2, expect2); // consumes the last, so the emitted drop runs from inside it

    PROVE(owners == 1,
          "exactly one caller is told it owns the cleanup -- never two, which would close "
          "a reused descriptor, and never zero, which would leak the handle");
    PROVE(cleanup_calls == 1, "and so the cleanup ran exactly once");
    PROVE(env_drops == 1,
          "the closure environment is released exactly once, by the body's own drop");

    PROVE(g_payload->rc == 1,
          "the payload was released exactly once, by the single caller that won");
    PROVE(payload_drops == 0, "the payload is not dropped while the harness still holds it");

    neon_release(g_payload);
    PROVE(payload_drops == 1, "the payload is dropped exactly once, and only at rc == 0");

    // Nothing else is freed by hand. The ref, the body and the environment must all have
    // been reclaimed by the code under test; --memory-leak-check is the assertion.
}

int main(void) {
    // Two racers on the owning ROOT context, armed body: the first wins, the second
    // learns the cleanup is already claimed. Root-context scenarios keep the sentinel
    // substitution covered; see the tractability note at the context stub.
    scenario(0, NULL, NULL, NEON_RESOURCE_OK, NULL, NEON_RESOURCE_RELEASED);
    // A take won first: both racers pass the gate and lose at the armed check.
    scenario(1, NULL, NULL, NEON_RESOURCE_RELEASED, NULL, NEON_RESOURCE_RELEASED);
    // A foreign fiber races the owning fiber and goes first: it loses at the gate,
    // before the armed check is even consulted, and disturbs nothing -- the owner still
    // wins afterwards.
    scenario(0, OWNER_FIBER, OTHER_FIBER, NEON_RESOURCE_NOT_OWNER, OWNER_FIBER,
             NEON_RESOURCE_OK);
    // The owner wins first, the foreign fiber races second: still NOT_OWNER, not
    // RELEASED -- the gate answers before the armed check, in that order.
    scenario(0, OWNER_FIBER, OWNER_FIBER, NEON_RESOURCE_OK, OTHER_FIBER,
             NEON_RESOURCE_NOT_OWNER);
    return 0;
}
