// Model: two consecutive `neon_resource_take` calls on one resource, and the payload slot
// either of them may have written.
//
// THE INVARIANT: `neon_resource_take` succeeds if and only if the resource is armed, and
// when it succeeds it yields the payload that went in, disarms, and ZEROES the source
// slot; a second take returns false and does not write `out` at all.
//
// The zeroing is the part worth a machine check, and it is not local reasoning: nothing at
// the take's own call site observes it. `neon_resource_finish` releases whatever is still
// in the payload slot, so a take that moved ownership out and left the bytes behind
// releases the same object twice -- once through the cleanup that received it, once
// through finish. That shipped. It ran clean against every `Resource[i64, E]` in the tree,
// because a scalar's witness has no `release` and the second one was a call through a NULL
// function pointer, and it use-after-freed the first `Resource[str, E]`.
//
// So the payload here is a COUNTED handle -- a `neon_header*` with a real retain/release
// witness -- and not a scalar. That is rule 7 and it is the entire reason this model
// exists: a second release is then a real `neon_release` on a real object, caught as an
// unbalanced reference count at the end of main. With a scalar payload this file would
// pass and prove nothing.
//
// The `model_drop` below is what codegen emits per instantiation, in the shape codegen
// emits it. It is harness, not code under test: every runtime function it calls is the
// real one.
//
// Verifies `src/resource.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Baseline: 478 properties, VERIFICATION SUCCESSFUL. Two mutations, both reverted.
//
// 1. `neon_resource_take` no longer zeroing the source slot -- the `memset` deleted, which
//    is the mistake to make, since the move looks complete without it. Failed on "take
//    zeroes the payload slot at the source, so neon_resource_finish cannot release bytes
//    whose ownership has already moved", on "a failing take leaves the zeroed slot zeroed",
//    and on "the payload was released exactly once, by the cleanup that received it", with
//    a deallocated-object dereference at the count check (8 of 463). This is the model's
//    reason for existing and the bug named in `src/resource.c`'s own comment: shipped, it
//    is silent for every scalar payload and a use-after-free the first time a
//    `Resource[str, E]` is dropped.
//
// 2. `neon_resource_take` no longer clearing `armed`. Failed on "take disarms the
//    resource, so nothing else can win the cleanup", "a second take fails: the cleanup was
//    already claimed", "armed is monotone: a failing take does not re-arm the resource",
//    and "the drop's own take failed, so cleanup did not run a second time" (6 of 472) --
//    the whole disarm half of the claim, caught four separate ways.
//
// NOT CAUGHT: deleting the `w->release` call in `neon_resource_finish` (0 of 441), and
// doubling it (0 of 497). Both are equivalent mutants rather than gaps: this model proves
// the payload slot is zero at every `finish`, so the release there is always
// `release(NULL)`. That release only has an effect on a drop that skipped the take, which
// is the defect `verify-resource-cleanup-runs-exactly-once` mutates and catches.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. AT MOST TWO CONSECUTIVE TAKES. `armed` is monotone -- `neon_resource_take` is the
//    only writer and only ever writes false -- so a third take is in exactly the state
//    the second one was, and the second is the one that proves the failing path writes
//    nothing.
//
// 2. EXACTLY ONE CONSUMING OPERATION runs: the final `neon_release`. Take consumes no
//    reference, which is why a run of them is affordable here at all. Adding a consuming
//    operation ahead of the last one is the expensive change, not a cheap one: a
//    `neon_release` CBMC cannot constant-fold leaves its object *symbolically* freed,
//    every later dereference carries that disjunction, and the drop recursion behind it
//    (release -> drop -> `neon_resource_finish` -> release -> ...) re-expands to the full
//    `--unwind` depth at each one. Measured on the model this was split out of: 0.45s to
//    over 300s for one extra consuming operation. Which operation ends the life, from
//    both states, is proved by `verify-resource-cleanup-runs-exactly-once`.
//
// 3. WHICH OPERATION RUNS THE CLEANUP is not claimed here, only that the ownership the
//    take moved out is not also released by `neon_resource_finish`. The exactly-once
//    property across all six public operations is the other model.
//
// 4. Out-of-memory does not appear as a *return*: `neon_alloc` traps rather than returning
//    NULL, so `neon_resource_new` has no failure path. `--malloc-may-fail
//    --malloc-fail-null` buys a check that the trap terminates rather than running on with
//    a NULL header, which the `_exit` stub encodes; a leak check cannot fire past a trap.
//
// 5. Payloads other than one counted pointer. `w->size` is read from the witness by the
//    code under test -- both `memcpy`s and the `memset` are sized by it -- so the sizing
//    arithmetic is exercised, just at a single size. A payload spanning several counted
//    fields, or one whose witness has a `retain` but no `release`, is not covered.
//
// 6. Concurrency. `armed` is a plain `bool` and the runtime is single-threaded; "a second
//    take" means a later one, not a simultaneous one.

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
// the shared tail. Reached here on the final release, after the harness has already taken
// the payload, so its take must fail and `neon_resource_finish` must find a zeroed slot.
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

int main(void) {
    neon_header* g_payload = (neon_header*)neon_alloc(0, payload_drop);
    neon_header* g_env = (neon_header*)neon_alloc(0, env_drop);

    neon_closure cleanup;
    cleanup.fn = (void*)model_cleanup;
    cleanup.env = g_env; // the resource takes ownership of this reference

    // The payload's single reference moves into the resource.
    neon_resource* r = neon_resource_new(&g_payload, &handle_witness, cleanup, model_drop);

    // A reference the harness keeps and releases last, so an imbalance is caught as a
    // count rather than as a use-after-free: `rc == 0` at the end is one release too
    // many, `rc == 2` is one too few. Both directions fail, and they fail at the point of
    // imbalance rather than once something later touches the freed bytes.
    neon_retain(g_payload);

    PROVE(r->armed, "a fresh resource is armed");
    PROVE(*(neon_header**)neon_resource_payload(r) == g_payload,
          "the payload is copied into the inline slot, sized by its witness");
    PROVE(g_payload->rc == 2,
          "the payload's reference moved into the resource; the other is the harness pin");

    // ---- the first take: the move-out ----

    neon_header* got = UNWRITTEN;
    bool mine = neon_resource_take(r, &got);

    PROVE(mine, "take succeeds on an armed resource");
    PROVE(got == g_payload, "take yields the payload that went in");
    PROVE(!r->armed, "take disarms the resource, so nothing else can win the cleanup");
    PROVE(*(neon_header**)neon_resource_payload(r) == NULL,
          "take zeroes the payload slot at the source, so neon_resource_finish cannot "
          "release bytes whose ownership has already moved");
    PROVE(g_payload->rc == 2,
          "a take moves the payload rather than copying it: the count is unchanged and "
          "the resource no longer holds a reference");
    PROVE(r->header.rc == 1, "a take consumes no reference to the resource");
    PROVE(r->cleanup.env == g_env, "the closure survives a take");

    // ---- the second take: must fail, and must hand out nothing ----

    neon_header* got2 = UNWRITTEN;
    bool mine2 = neon_resource_take(r, &got2);

    PROVE(!mine2, "a second take fails: the cleanup was already claimed");
    PROVE(got2 == UNWRITTEN,
          "a failing take does not write out at all, so the second caller never receives "
          "a payload it would then clean up");
    PROVE(*(neon_header**)neon_resource_payload(r) == NULL,
          "a failing take leaves the zeroed slot zeroed");
    PROVE(!r->armed, "armed is monotone: a failing take does not re-arm the resource");
    PROVE(r->header.rc == 1, "a failing take consumes no reference either");

    // We own the payload now and so owe it a cleanup, exactly as the emitted code does.
    // Borrowing `r->cleanup.env` is safe: the resource is still alive.
    ((cleanup_fn)r->cleanup.fn)(r->cleanup.env, got);
    PROVE(cleanup_calls == 1, "the winning take's caller ran the cleanup");
    PROVE(g_payload->rc == 1, "and that cleanup consumed the payload it was handed");

    // ---- the one consuming operation: the last release, running the emitted drop ----
    //
    // This is where the zeroing pays off. `neon_resource_finish` calls
    // `w->release(payload_slot)` unconditionally, which is `neon_release(NULL)` only
    // because the slot was zeroed. Were it not, the count below would be 0.
    neon_release((neon_header*)r);

    PROVE(cleanup_calls == 1,
          "the drop's own take failed, so cleanup did not run a second time");
    PROVE(env_drops == 1, "the closure environment is released exactly once");
    PROVE(g_payload->rc == 1,
          "the payload was released exactly once, by the cleanup that received it: "
          "neon_resource_finish released the zeroed slot, not the moved-out payload");
    PROVE(payload_drops == 0, "the payload is not dropped while the harness still holds it");

    neon_release(g_payload);
    PROVE(payload_drops == 1, "the payload is dropped exactly once, and only at rc == 0");

    // Nothing else is freed by hand. The resource and the environment must both have been
    // reclaimed by the code under test; --memory-leak-check is the assertion.
    return 0;
}
