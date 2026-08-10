// Model: `neon_wcopy_resource` inside and outside the transfer bracket, from an owning
// and from a non-owning source, with the claim on the far side and both refs' drops.
//
// THE INVARIANT: at every point in a resource's life, EXACTLY ONE owning ref exists for
// an armed body. A bracketed copy of the owning ref is a TRANSFER -- the source ref is
// demoted, the minted ref carries the baton, and the body goes unowned (in flight) until
// the owning ref's first gated use claims it for its new context. An unbracketed copy,
// or any copy from a non-owning source, mints a NON-owning ref and moves nothing. The
// demoted source's drop is inert; the owning ref's drop runs cleanup exactly once,
// whether or not a claim happened in between.
//
// This is the baton, and every clause is a silent failure mode when wrong. Two owning
// refs (a missed demotion) is cleanup twice -- a double `close` on a reused descriptor,
// from two different fibers. Zero owning refs (a mint that forgets the flag) is cleanup
// never -- a leaked handle no test observes. A copy that moves the baton WITHOUT the
// bracket turns every innocent alias into a theft: the sender keeps using a resource
// whose cleanup now belongs to someone else. And a claim that does not require the
// owning ref lets any fiber that was merely shown an in-flight resource take ownership
// of it -- the gate would be a report, not a gate.
//
// Rule 7: the payload is a COUNTED handle with a real retain/release witness, never a
// scalar, so "cleanup twice" is a real double release caught as a count, and "cleanup
// never" is a real leak caught by --memory-leak-check.
//
// The `model_drop` below is what codegen emits per instantiation as the REF-drop, in the
// shape codegen emits it -- and this model is where its `r->owning` guard earns its keep:
// the demoted ref and the minted aliases all run the same drop and must all be inert.
// Harness, not code under test: every runtime function it calls is real.
//
// Verifies `src/resource.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Mutations are applied to a scratch COPY of `src/resource.c` and the model re-run
// against the copy; the shipping source is never edited. Baseline: 916 properties,
// VERIFICATION SUCCESSFUL. Five transfer mutations, each discarded with its scratch
// copy, plus three cross-witnesses.
//
// 1. THE DEMOTION DELETED -- `((neon_resource*)r)->owning = false;` removed from the
//    transfer arm of `neon_wcopy_resource`, so a transfer leaves TWO owning refs. Failed
//    on "a transfer demotes the source ref", "exactly one owning ref exists the moment
//    the transfer completes", "a demoted ref's drop is inert: it runs no cleanup and
//    takes nothing" -- the doubled ref's drop DOES clean up, first -- and "and leaves
//    the body armed for the owning ref" (5 of 910). The second owning drop then finds
//    the body disarmed, which is why the catch is the named inertness claims rather
//    than a double release: exactly-once still emerges from `armed`, and the model
//    pins the invariant that makes that emergence sound.
//
// 2. THE MINT ALWAYS NON-OWNING -- `neon_resource_ref_new(b, r->header.drop, moving)`
//    given `false`, so a transfer demotes the source and hands the baton to no one.
//    Failed on "a transfer's mint carries the baton", "exactly one owning ref exists
//    ...", the whole claim path (an ownerless in-flight body cannot be claimed: the far
//    side's get answers NOT_OWNER, not OK), "the owning ref's drop ran cleanup exactly
//    once ..." -- it never ran -- and a count underflow behind the unretained read
//    (12 of 916).
//
// 3. THE BRACKET CONDITION INVERTED -- `moving = !t_transfer && r->owning`, so innocent
//    aliases steal the baton and transfer edges mint powerless copies. The largest
//    failure set in the campaign, and in both directions at once: the bracketed
//    scenarios lose their transfer ("demotes the source", "mint carries the baton", the
//    claim path), the unbracketed scenario loses its safety ("an unbracketed copy does
//    not move the baton", "a plain alias is non-owning", "using a plain alias from
//    another fiber is a named error"), and the ledger fails behind both, leak included
//    (28 of 916).
//
// 4. THE CLAIM UNCONDITIONAL -- the gate's `owner == NULL && r->owning` reduced to
//    `owner == NULL`, so any fiber shown an in-flight resource can claim it through a
//    non-owning alias. Failed on "a non-owning alias cannot claim an in-flight body:
//    the gate requires the baton, not just the name" -- the alias's get answers OK
//    instead of NOT_OWNER -- on "a failed claim leaves the body in flight", and on the
//    payload ledger plus the leak behind the stolen read (6 of 910).
//
// 5. THE HANDOFF HALF-DONE -- the `owner = NULL` store removed from the transfer arm, so
//    the body stays owned by the sender while the baton travels. Failed on "a transfer
//    leaves the body unowned -- in flight -- ..." in both bracketed scenarios and on the
//    far side's entire claim path: the new owner's first use answers NOT_OWNER on its
//    own resource (9 of 916).
//
// Cross-witnesses, caught here as well as in the models that name them:
// `neon_resource_take` not zeroing (6 of 901 -- the body's drop releases what the
// owning drop's take moved out) and `neon_resource_get` not retaining (6 of 897), both
// through the payload ledger. NOT CAUGHT, and correctly so: `neon_resource_take` not
// clearing `armed` (0 of 916) -- every life here ends through exactly one owning drop
// and no second taker exists, so nothing reads the stale flag; the RELEASED answers and
// racing takes that expose it are `verify-resource-take-moves-the-payload-out-once`,
// `verify-resource-cleanup-runs-exactly-once` and the disarm race's ground.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. THE BRACKET IS DRIVEN BY THE HARNESS, as the three transfer edges drive it: begin,
//    one copy, end, never nested. `neon_transfer_begin`'s own assert that edges never
//    nest is compiled in (a plain `assert`, a property under CBMC) but only its
//    non-nested path is reached. What a SECOND copy inside one bracket does -- two
//    resources riding one channel send both transfer, which is the intended semantics --
//    is exercised only as one-copy-per-bracket here, twice in scenario C; the bracket
//    flag is read per copy and unchanged between them, so the argument that this
//    generalises is the same one the code makes.
//
// 2. THE CONTEXTS ARE FIBER-OBJECT IDENTITIES, never the root, in every scenario that
//    compares contexts -- the tractability note at the context stub says why (a root
//    sentinel comparison against a different context does not fold in symex and the drop
//    recursion re-expands behind the unpruned gate branch; measured at over 240s in the
//    disarm model). The root-context paths through `neon_resource_ctx` are covered by
//    the root-only models; nothing in the transfer logic reads the context except
//    through it.
//
// 3. DROP ORDERS: the demoted source dies before the owning ref in scenarios A and B,
//    after it in scenario C (where the demoted `r` is the last ref standing and the
//    body's death lands on it). The interleaving where an alias outlives the owning
//    ref's cleanup and then reads RELEASED through the gate is the disarm and get
//    models' ground.
//
// 4. CROSS-THREAD delivery. The transfer bracket is thread-local and the handoff's
//    visibility argument (neon/resource.h) is about publish edges this model does not
//    build; "the far side" here is a context switch in one sequential execution. The
//    body's shared count is exercised, but single-threaded.
//
// 5. Out-of-memory does not appear as a *return*: `neon_alloc` traps rather than
//    returning NULL. `--malloc-may-fail --malloc-fail-null` buys a check that the trap
//    terminates rather than running on with a NULL header, which the `_exit` stub
//    encodes; a leak check cannot fire past a trap.
//
// 6. Payloads other than one counted pointer, as in every model of this set.

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

// The current execution context, from the fiber sources this model does not build. The
// sending fiber and the receiving fiber are static identities, never dereferenced.
//
// TRACTABILITY: every scenario stands on OBJECT identities, never on the root (NULL).
// Symex folds `&owner_ctx_obj != &other_ctx_obj` (and `NULL == NULL` for the in-flight
// checks) and prunes the gate's dead branch; a comparison involving the root sentinel
// `(neon_fiber*)1` against a different context is not folded, both gate branches stay
// live, and the drop recursion re-expands behind them -- measured at under 1s against
// over 240s in `verify-resource-disarm-picks-exactly-one-winner`.
static char owner_ctx_obj;
static char other_ctx_obj;
#define OWNER_FIBER ((neon_fiber*)&owner_ctx_obj)
#define OTHER_FIBER ((neon_fiber*)&other_ctx_obj)
static neon_fiber* current_fiber;
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
    // Zeroed by `neon_resource_take` on the moved-out path, so this is `release(NULL)`
    // there -- and a second live release of a real object if any drop double-runs, which
    // the counts catch.
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
// In this model the same drop runs for the owning ref, the demoted source and the plain
// aliases -- the `r->owning` guard is what keeps all but one of them inert.
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

static neon_header* g_payload;

// Build one resource on OWNER_FIBER's context, pin its payload, and hand back the
// owning ref. Every scenario starts here.
static neon_resource* build(void) {
    payload_drops = 0;
    env_drops = 0;
    cleanup_calls = 0;
    current_fiber = OWNER_FIBER;

    g_payload = (neon_header*)neon_alloc(0, payload_drop);
    neon_header* g_env = (neon_header*)neon_alloc(0, env_drop);

    neon_closure cleanup;
    cleanup.fn = (void*)model_cleanup;
    cleanup.env = g_env; // the body takes ownership of this reference

    neon_resource* r = neon_resource_new(&g_payload, &handle_witness, cleanup, model_drop);
    neon_retain(g_payload); // the harness pin: imbalances read as counts, not as UAFs

    PROVE(r->owning, "the ref neon_resource_new returns carries the baton");
    PROVE(r->body->owner == OWNER_FIBER, "and the body is owned by its creating context");
    PROVE(r->body->armed, "a fresh body is armed");
    PROVE(r->body->header.rc == 1, "one ref, one count on the body");
    return r;
}

// The end-of-scenario ledger: cleanup ran exactly once, the env died with the body, the
// payload was released exactly once by whoever ran the cleanup.
static void settle(void) {
    PROVE(cleanup_calls == 1,
          "the owning ref's drop ran cleanup exactly once over the whole life -- however "
          "many refs were minted, wherever the baton travelled, claim or no claim");
    PROVE(env_drops == 1,
          "the closure environment is released exactly once, by the body's own drop");
    PROVE(g_payload->rc == 1,
          "the payload was released exactly once, by whoever ran the cleanup");
    PROVE(payload_drops == 0, "the payload is not dropped while the harness still holds it");
    neon_release(g_payload);
    PROVE(payload_drops == 1, "the payload is dropped exactly once, and only at rc == 0");
    // Every ref and the body must have been reclaimed by the code under test;
    // --memory-leak-check is the assertion.
}

// ---- scenario A: a bracketed transfer, with and without a claim in between ----
//
// `claim` is a literal at each call site: made nondeterministic, the claimed and
// in-flight owner values merge into one symbolic pointer ahead of both drops.
static void scenario_transfer(int claim) {
    neon_resource* r = build();
    neon_resource_body* b = r->body;

    // The transfer edge: what a channel send, a `move` closure or a task return does
    // around its one copy of the resource slot.
    neon_transfer_begin();
    neon_resource* nr;
    neon_wcopy_resource(&r, &nr);
    neon_transfer_end();

    PROVE(!r->owning, "a transfer demotes the source ref: the name stays, the baton goes");
    PROVE(nr->owning, "a transfer's mint carries the baton");
    PROVE(nr->body == b, "to the same body: a resource never copies, only its refs do");
    PROVE(r->owning + nr->owning == 1,
          "exactly one owning ref exists the moment the transfer completes");
    PROVE(b->owner == NULL,
          "a transfer leaves the body unowned -- in flight -- until the far side's first "
          "use claims it; NULL means exactly this, never the root context");
    PROVE(b->armed, "the baton moved; the cleanup obligation did not fire");
    PROVE(b->header.rc == 2, "two refs, two counts on the body");

    if (claim) {
        // The far side's first use: a gated read from the receiving fiber. The gate
        // finds the body unowned, sees the owning ref, and claims.
        current_fiber = OTHER_FIBER;
        neon_retain((neon_header*)nr); // pay for the get; the drop below needs the other
        neon_header* got = UNWRITTEN;
        int64_t code = neon_resource_get(nr, &got);
        PROVE(code == NEON_RESOURCE_OK,
              "the owning ref's first gated use on the far side is answered OK");
        PROVE(b->owner == OTHER_FIBER,
              "and claims the in-flight body for the claiming context");
        PROVE(got == g_payload, "an owned read of the payload that went in");
        neon_release(got);
    }

    // The demoted source dies on the sending side, as it does when the sender's frame
    // unwinds. Its drop must be inert: no cleanup, no disarm, just a count.
    current_fiber = OWNER_FIBER;
    neon_release((neon_header*)r);
    PROVE(cleanup_calls == 0,
          "a demoted ref's drop is inert: it runs no cleanup and takes nothing");
    PROVE(b->armed, "and leaves the body armed for the owning ref");
    PROVE(b->header.rc == 1, "one ref remains, and it is the owning one");
    PROVE(nr->owning, "still carrying the baton");

    // The owning ref dies on the receiving side -- with the body still in flight when no
    // claim happened, which is exactly a channel discarding an undelivered resource.
    current_fiber = claim ? OTHER_FIBER : OWNER_FIBER;
    neon_release((neon_header*)nr);
    settle();
}

// ---- scenario B: an unbracketed copy is a plain mint, and authority does not travel ----

static void scenario_plain_mint(void) {
    neon_resource* r = build();
    neon_resource_body* b = r->body;

    neon_resource* alias;
    neon_wcopy_resource(&r, &alias); // no bracket: any non-edge crossing

    PROVE(r->owning, "an unbracketed copy does not move the baton");
    PROVE(!alias->owning, "a plain alias is non-owning: the name travels, the authority does not");
    PROVE(b->owner == OWNER_FIBER, "the body stays owned by the source's context");
    PROVE(b->header.rc == 2, "two refs, two counts on the body");

    // The alias crosses to another fiber and is used there: the gate answers NOT_OWNER,
    // and the read hands back nothing.
    current_fiber = OTHER_FIBER;
    neon_header* got = UNWRITTEN;
    int64_t code = neon_resource_get(alias, &got); // consumes the alias
    PROVE(code == NEON_RESOURCE_NOT_OWNER,
          "using a plain alias from another fiber is a named error, not a shared handle");
    PROVE(got == UNWRITTEN, "and the losing read writes nothing");
    PROVE(b->armed && b->owner == OWNER_FIBER,
          "and disturbs nothing: still armed, still the creator's");

    // The owning ref dies at home and runs cleanup, exactly once.
    current_fiber = OWNER_FIBER;
    neon_release((neon_header*)r);
    settle();
}

// ---- scenario C: a copy from a NON-owning source moves nothing, bracket or not, and a
// non-owning alias cannot claim an in-flight body ----

static void scenario_alias_transfer(void) {
    neon_resource* r = build();
    neon_resource_body* b = r->body;

    neon_resource* alias;
    neon_wcopy_resource(&r, &alias); // plain mint: non-owning

    // A transfer edge whose source is the ALIAS: bracketed, but the source holds no
    // baton, so nothing moves.
    neon_transfer_begin();
    neon_resource* from_alias;
    neon_wcopy_resource(&alias, &from_alias);
    neon_transfer_end();

    PROVE(!from_alias->owning,
          "a transfer from a non-owning source moves nothing: the mint is non-owning");
    PROVE(!alias->owning && r->owning,
          "and demotes nothing: the baton stays exactly where it was");
    PROVE(b->owner == OWNER_FIBER, "the body stays owned -- this was no handoff");
    PROVE(b->header.rc == 3, "three refs, three counts on the body");

    // Now the real transfer: the owning ref rides an edge, the body goes in flight.
    neon_transfer_begin();
    neon_resource* moved;
    neon_wcopy_resource(&r, &moved);
    neon_transfer_end();
    PROVE(!r->owning && moved->owning && b->owner == NULL,
          "the owning ref's bracketed copy is the transfer: source demoted, mint owning, "
          "body in flight");

    // The alias, on another fiber, tries to use the in-flight body. The claim requires
    // the OWNING ref; a non-owning alias must not be able to take ownership.
    current_fiber = OTHER_FIBER;
    neon_header* got = UNWRITTEN;
    int64_t code = neon_resource_get(alias, &got); // consumes the alias
    PROVE(code == NEON_RESOURCE_NOT_OWNER,
          "a non-owning alias cannot claim an in-flight body: the gate requires the "
          "baton, not just the name");
    PROVE(got == UNWRITTEN, "and the losing read writes nothing");
    PROVE(b->owner == NULL, "a failed claim leaves the body in flight");
    PROVE(b->armed, "and armed");

    // The rightful receiver's first use claims it -- and its get consumes the owning
    // ref's only count, so the ref-drop runs from inside the get: cleanup fires with the
    // owned read still in the caller's hand, which is what last-use ARC means.
    neon_header* got2 = UNWRITTEN;
    int64_t code2 = neon_resource_get(moved, &got2);
    PROVE(code2 == NEON_RESOURCE_OK, "the owning ref claims and reads in one gated use");
    PROVE(got2 == g_payload, "an owned read of the payload that went in");
    PROVE(cleanup_calls == 1,
          "the get consumed the owning ref's last count, so the ref-drop ran cleanup "
          "right there -- after the read was retained out");
    neon_release(got2);

    // The powerless refs die last, on their own side; both drops are inert, and the
    // body's memory death lands on whichever goes second.
    current_fiber = OWNER_FIBER;
    neon_release((neon_header*)from_alias);
    neon_release((neon_header*)r);
    settle();
}

int main(void) {
    scenario_transfer(0); // transfer, no claim: the owning drop cleans up an in-flight body
    scenario_transfer(1); // transfer, claim, then both drops
    scenario_plain_mint();
    scenario_alias_transfer();
    return 0;
}
