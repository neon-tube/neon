// Model: two consecutive `neon_resource_take` calls on one resource BODY, and the payload
// slot either of them may have written.
//
// THE INVARIANT: `neon_resource_take` succeeds if and only if the body is armed, and when
// it succeeds it yields the payload that went in, disarms, and ZEROES the source slot; a
// second take returns false and does not write `out` at all.
//
// Take is now the ungated move-out on the BODY -- the shared half of the body/ref split --
// not on the per-holder ref. Its callers are the emitted ref-drop (already the unique
// holder of the baton) and the gated `neon_resource_disarm`; the gate is those callers'
// business, so this model drives take bare, on the body, exactly as they do.
//
// The zeroing is the part worth a machine check, and it is not local reasoning: nothing at
// the take's own call site observes it. `neon_resource_body_drop` -- the body's own memory
// death, which runs when the last REF dies -- releases whatever is still in the payload
// slot, so a take that moved ownership out and left the bytes behind releases the same
// object twice: once through the cleanup that received it, once through the body's drop.
// That shipped, in the single-object design this code grew out of. It ran clean against
// every `Resource[i64, E]` in the tree, because a scalar's witness has no `release` and
// the second one was a call through a NULL function pointer, and it use-after-freed the
// first `Resource[str, E]`.
//
// So the payload here is a COUNTED handle -- a `neon_header*` with a real retain/release
// witness -- and not a scalar. That is rule 7 and it is the entire reason this model
// exists: a second release is then a real `neon_release` on a real object, caught as an
// unbalanced reference count at the end of main. With a scalar payload this file would
// pass and prove nothing.
//
// The `model_drop` below is what codegen emits per instantiation as the REF-drop, in the
// shape codegen emits it: `if (owning && take) cleanup; ref_finish`. It is harness, not
// code under test: every runtime function it calls is the real one.
//
// Verifies `src/resource.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Mutations are applied to a scratch COPY of `src/resource.c` and the model re-run against
// the copy; the shipping source is never edited. Baseline: 679 properties, VERIFICATION
// SUCCESSFUL. Two mutations, both discarded with the scratch copy.
//
// 1. `neon_resource_take` no longer zeroing the source slot -- the `memset` deleted, which
//    is the mistake to make, since the move looks complete without it. Failed on "take
//    zeroes the payload slot at the source, so the body's own drop cannot release bytes
//    whose ownership has already moved", on "a failing take leaves the zeroed slot
//    zeroed", and on "the payload was released exactly once, by the cleanup that received
//    it", with deallocated-object dereferences and a count underflow inside
//    `neon_release` at the count check (8 of 664). This is the model's reason for
//    existing: shipped, it is silent for every scalar payload and a use-after-free the
//    first time a `Resource[str, E]` is dropped.
//
// 2. `neon_resource_take` no longer clearing `armed`. Failed on "take disarms the body, so
//    nothing else can win the cleanup", "a second take fails: the cleanup was already
//    claimed", "a failing take does not write out at all", "armed is monotone: a failing
//    take does not re-arm the body", "the drop's own take failed, so cleanup did not run
//    a second time", and "cleanup receives a payload" -- the drop's second winning take
//    hands the cleanup the zeroed slot's NULL (6 of 679). The whole disarm half of the
//    claim, caught six separate ways.
//
// NOT CAUGHT: deleting the `w->release` call in `neon_resource_body_drop` (0 of 648), and
// doubling it (0 of 698). Both are equivalent mutants here rather than gaps: this model
// proves the payload slot is zero at every body drop, so the release there is always
// `release(NULL)`. That release only has an effect on a ref-drop that skipped the take,
// which is the defect `verify-resource-cleanup-runs-exactly-once` mutates and catches.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. AT MOST TWO CONSECUTIVE TAKES. `armed` is monotone -- `neon_resource_take` is the
//    only writer and only ever writes false -- so a third take is in exactly the state
//    the second one was, and the second is the one that proves the failing path writes
//    nothing.
//
// 2. EXACTLY ONE CONSUMING OPERATION runs: the final `neon_release` of the one ref. Take
//    consumes no reference, which is why a run of them is affordable here at all. Adding
//    a consuming operation ahead of the last one is the expensive change, not a cheap
//    one: a `neon_release` CBMC cannot constant-fold leaves its object *symbolically*
//    freed, every later dereference carries that disjunction, and the drop recursion
//    behind it (release -> ref-drop -> `neon_resource_ref_finish` -> shared release ->
//    body drop -> ...) re-expands to the full `--unwind` depth at each one. Measured on
//    the single-object predecessor of this model set: 0.45s to over 300s for one extra
//    consuming operation. Which operation ends the life, from both states, is proved by
//    `verify-resource-cleanup-runs-exactly-once`.
//
// 3. ONE REF, so the body dies with the ref that created it. Take does not touch either
//    count -- asserted here on both the ref and the body -- and a second ref changes only
//    where the body's death lands, which is `verify-a-transfer-leaves-exactly-one-owning-
//    ref`'s ground.
//
// 4. THE OWNER GATE is not exercised: take is UNGATED by design, and this model calls it
//    the way its two real callers do -- already past the gate. The gate itself (claim,
//    NOT_OWNER, RELEASED) is `verify-resource-disarm-picks-exactly-one-winner` and the
//    transfer model. The harness stays on the root context throughout.
//
// 5. Out-of-memory does not appear as a *return*: `neon_alloc` traps rather than returning
//    NULL, so `neon_resource_new` has no failure path. `--malloc-may-fail
//    --malloc-fail-null` buys a check that the trap terminates rather than running on with
//    a NULL header, which the `_exit` stub encodes; a leak check cannot fire past a trap.
//
// 6. Payloads other than one counted pointer. `w->size` is read from the witness by the
//    code under test -- both `memcpy`s and the `memset` are sized by it -- so the sizing
//    arithmetic is exercised, just at a single size. A payload spanning several counted
//    fields, or one whose witness has a `retain` but no `release`, is not covered.
//
// 7. Concurrency. `armed` is a plain `bool` -- the baton handoff argument in
//    neon/resource.h is why -- and "a second take" here means a later one, not a
//    simultaneous one. The body's shared count is exercised only single-threaded.

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
// is the root context -- resource.c substitutes its own sentinel for it, so NULL never
// reaches an owner comparison. This model never leaves the root context.
neon_fiber* neon_fiber_current(void) { return NULL; }

// Reached only for a closure environment flagged NEON_ALLOC_ARENA, which the CBMC
// `neon_alloc` never sets -- asserted, not assumed, so a change to that allocator that
// starts routing envs here fails the model instead of silently exercising a stub.
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

// The payload's witness: rule 7. `size` is one pointer and retain/release forward to the
// lifecycle, the shape codegen emits for a `Resource[str, E]` whose payload carries a
// counted owner.
static void handle_retain(void* elem) {
    neon_retain(*(neon_header**)elem);
}

static void handle_release(void* elem) {
    // The slot is zeroed by `neon_resource_take`, so on the moved-out path this is
    // `neon_release(NULL)` -- a no-op. If that zeroing ever goes away this becomes a
    // second release of a live object, and the payload's count at the end of main
    // catches it. That is the whole mechanism this model turns on.
    neon_release(*(neon_header**)elem);
}

static bool handle_eq(const void* a, const void* b) {
    return *(neon_header* const*)a == *(neon_header* const*)b;
}

// `neon_resource_new` CONSUMES the caller's payload: it deep-copies it into the body
// through `copy` and then releases the original through `release`. A counted handle's
// copy is alias-and-retain -- without it the memcpy fallback would move the pointer and
// the release would destroy the reference that just moved in.
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

// What codegen emits as the resource's REF-drop: if this ref carries the baton and the
// body is still armed, take the payload and run cleanup; land in the shared tail either
// way. Reached here on the final release, after the harness has already taken the
// payload, so its take must fail and the body's drop must find a zeroed slot.
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

int main(void) {
    neon_header* g_payload = (neon_header*)neon_alloc(0, payload_drop);
    neon_header* g_env = (neon_header*)neon_alloc(0, env_drop);

    neon_closure cleanup;
    cleanup.fn = (void*)model_cleanup;
    cleanup.env = g_env; // the body takes ownership of this reference

    // The payload's single reference moves into the body.
    neon_resource* r = neon_resource_new(&g_payload, &handle_witness, cleanup, model_drop);
    neon_resource_body* b = r->body;

    // A reference the harness keeps and releases last, so an imbalance is caught as a
    // count rather than as a use-after-free: `rc == 0` at the end is one release too
    // many, `rc == 2` is one too few. Both directions fail, and they fail at the point of
    // imbalance rather than once something later touches the freed bytes.
    neon_retain(g_payload);

    PROVE(b->armed, "a fresh body is armed");
    PROVE(r->owning, "the ref neon_resource_new returns carries the baton");
    PROVE(*(neon_header**)neon_resource_body_payload(b) == g_payload,
          "the payload is copied into the body's inline slot, sized by its witness");
    PROVE(g_payload->rc == 2,
          "the payload's reference moved into the body; the other is the harness pin");
    PROVE(r->header.rc == 1, "one holder, one ref");
    PROVE(b->header.rc == 1, "and the body's shared count is exactly that ref's");

    // ---- the first take: the move-out ----

    neon_header* got = UNWRITTEN;
    bool mine = neon_resource_take(b, &got);

    PROVE(mine, "take succeeds on an armed body");
    PROVE(got == g_payload, "take yields the payload that went in");
    PROVE(!b->armed, "take disarms the body, so nothing else can win the cleanup");
    PROVE(*(neon_header**)neon_resource_body_payload(b) == NULL,
          "take zeroes the payload slot at the source, so the body's own drop cannot "
          "release bytes whose ownership has already moved");
    PROVE(g_payload->rc == 2,
          "a take moves the payload rather than copying it: the count is unchanged and "
          "the body no longer holds a reference");
    PROVE(r->header.rc == 1 && b->header.rc == 1,
          "a take touches neither the ref's count nor the body's");
    PROVE(b->cleanup.env == g_env, "the closure survives a take");

    // ---- the second take: must fail, and must hand out nothing ----

    neon_header* got2 = UNWRITTEN;
    bool mine2 = neon_resource_take(b, &got2);

    PROVE(!mine2, "a second take fails: the cleanup was already claimed");
    PROVE(got2 == UNWRITTEN,
          "a failing take does not write out at all, so the second caller never receives "
          "a payload it would then clean up");
    PROVE(*(neon_header**)neon_resource_body_payload(b) == NULL,
          "a failing take leaves the zeroed slot zeroed");
    PROVE(!b->armed, "armed is monotone: a failing take does not re-arm the body");
    PROVE(r->header.rc == 1 && b->header.rc == 1, "a failing take touches no count either");

    // We own the payload now and so owe it a cleanup, exactly as the emitted code does.
    // Borrowing `b->cleanup.env` is safe: the ref still holds the body.
    ((cleanup_fn)b->cleanup.fn)(b->cleanup.env, got);
    PROVE(cleanup_calls == 1, "the winning take's caller ran the cleanup");
    PROVE(g_payload->rc == 1, "and that cleanup consumed the payload it was handed");

    // ---- the one consuming operation: the last release, running the emitted ref-drop ----
    //
    // This is where the zeroing pays off. The ref-drop's own take fails, and
    // `neon_resource_ref_finish` releases the body, whose drop calls
    // `w->release(payload_slot)` unconditionally -- `neon_release(NULL)` only because the
    // slot was zeroed. Were it not, the count below would be 0.
    neon_release((neon_header*)r);

    PROVE(cleanup_calls == 1,
          "the drop's own take failed, so cleanup did not run a second time");
    PROVE(env_drops == 1, "the body's drop released the closure environment exactly once");
    PROVE(g_payload->rc == 1,
          "the payload was released exactly once, by the cleanup that received it: the "
          "body's drop released the zeroed slot, not the moved-out payload");
    PROVE(payload_drops == 0, "the payload is not dropped while the harness still holds it");

    neon_release(g_payload);
    PROVE(payload_drops == 1, "the payload is dropped exactly once, and only at rc == 0");

    // Nothing else is freed by hand. The ref, the body and the environment must all have
    // been reclaimed by the code under test; --memory-leak-check is the assertion.
    return 0;
}
