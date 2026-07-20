// Model: `neon_resource_get` on a live resource and on one whose payload has already been
// taken, with the reference count of the payload checked on both sides of the call.
//
// THE INVARIANT: `neon_resource_get` hands back an OWNED reference to the payload -- one
// the caller must release -- and does NOT disarm the resource. On a resource whose payload
// has already gone it returns false and does not write `out`.
//
// Both halves are silent when wrong. If `get` handed back a borrowed reference while the
// caller treated it as owned -- the convention every other reader in this ABI follows --
// the payload would be released once too often and the failure would land somewhere else
// entirely, on whoever still held it. If it leaked a retain, nothing would ever fail; the
// handle would simply never be closed. And if `get` disarmed, a read would silently
// consume the cleanup: the resource would still look alive to its holder, but the drop
// would find it disarmed and the cleanup would never run.
//
// Rule 7 is what makes the first half checkable at all. The payload is a COUNTED handle --
// a `neon_header*` with a real retain/release witness -- so `get`'s retain is an
// observable increment and the caller's release an observable decrement. With a scalar
// payload the witness has no `retain` and no `release`, `neon_resource_get`'s
// `if (r->w->retain)` is simply not taken, and every claim below is vacuous. That is not
// hypothetical: every `Resource[...]` in the tree held a scalar when this code
// use-after-freed, which is exactly why the first `Resource[str, E]` found it.
//
// The `model_drop` below is what codegen emits per instantiation, in the shape codegen
// emits it. It is harness, not code under test: every runtime function it calls is real.
//
// Verifies `src/resource.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Baseline: 554 properties, VERIFICATION SUCCESSFUL. Four mutations, each reverted.
//
// 1. The `w->retain(out)` deleted from `neon_resource_get`, so the read hands back a
//    borrow while every caller treats it as owned. Failed on "get hands back an OWNED
//    reference: it retains the payload through the witness before returning it, so the
//    caller owes exactly one release", on "and giving that reference back returns the count
//    to where it was: get is balanced against one release, not a leak and not a borrow",
//    and on "the payload was released exactly once, by whoever ran the cleanup", with a
//    deallocated-object dereference at the count check (8 of 535). Shipped, every `get`
//    followed by the release codegen emits for an owned value frees the resource's own
//    payload out from under it.
//
// 2. `neon_resource_get` additionally disarming -- the plausible confusion of a read with a
//    move. Failed on exactly the two claims that forbid it: "get does NOT disarm: a read
//    never claims the cleanup out from under the drop" and "cleanup ran exactly once: the
//    drop still found the resource armed when only a get had happened" (2 of 554). Shipped,
//    reading a resource silently cancels its cleanup: the handle is never closed and
//    nothing reports it.
//
// 3. `neon_resource_take` no longer clearing `armed`. Failed on seven, including "get
//    reports liveness from the armed flag", "get does NOT disarm", and "get yields the
//    payload that went in" (7 of 548).
//
// 4. `neon_resource_take` no longer zeroing the source slot -- and separately, the emitted
//    drop reading the payload slot directly instead of taking it (the historical bug; see
//    `verify-resource-cleanup-runs-exactly-once`, which owns that mutation). Both failed on
//    "the payload was released exactly once, by whoever ran the cleanup -- get's owned
//    reference was given back and released nothing extra" (6 of 539 and 6 of 566), so this
//    model is a second witness to the double free even though it is not the one that
//    names it.
//
// NOT CAUGHT: dropping the env retain in `neon_resource_cleanup` (0 of 554) -- this model
// never fetches the closure; that is `verify-resource-cleanup-retains-the-env-before-
// releasing`. Deleting or doubling the payload release in `neon_resource_finish`
// (0 of 517, 0 of 573); see the note in `verify-resource-take-moves-the-payload-out-once`.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. `get` CONSUMES A REFERENCE TO THE RESOURCE, and the header comment says otherwise.
//    `runtime/include/neon/resource.h:49` describes `neon_resource_get` as "Read the
//    payload without consuming the resource", but `runtime/src/resource.c:53` ends with
//    `neon_release((neon_header*)r)`, and the comment at `resource.c:40` -- "These consume
//    `r`, like every other native taking a counted pointer" -- says so explicitly. The
//    implementation is the truth and is what this model drives: the harness holds a second
//    reference so the resource survives the call, and asserts the count dropped by one.
//    The stale sentence in the header is a documentation defect, not a behavioural one;
//    "does not consume" in it is presumably a leftover from before the natives were made
//    uniform. What is genuinely claimed by both, and proved here, is that get does not
//    *disarm* -- it consumes a reference, not the payload.
//
// 2. ONE `get` PER LIFE. `neon_resource_get` writes nothing -- it reads `armed`, copies
//    the payload out and retains it -- so a second get from the same state differs from
//    the first only in the reference counts, which is what the lifecycle models cover.
//    Both *states* are driven here, which is the distinction that matters, since `armed`
//    is the only thing get branches on.
//
// 3. TWO CONSUMING OPERATIONS, and no more: the `get` and the final release. That is a
//    performance bound and sequence DEPTH is the expensive dimension -- a `neon_release`
//    CBMC cannot constant-fold leaves its object symbolically freed, every later
//    dereference carries the disjunction, and the drop recursion behind it re-expands to
//    the full `--unwind` depth at each one. One extra consuming operation took the model
//    this was split out of from 0.45s to over 300s. Both releases here are foldable (rc
//    2 -> 1, then 1 -> 0 with a concrete drop), which is why two fit. Take counts are
//    literals at each call site for the same reason: made nondeterministic, armed and
//    disarmed merge into one symbolic flag ahead of the branches -- under 1s to over 5
//    minutes for identical coverage.
//
// 4. Which operation ends a life, over all six public entry points, is
//    `verify-resource-cleanup-runs-exactly-once`, not this model.
//
// 5. Payloads other than one counted pointer. `w->size` is read from the witness by the
//    `memcpy` in `neon_resource_get`, so the sizing is exercised, but only at one size;
//    a witness with a `release` and no `retain` (or the reverse) is not covered, and it
//    is exactly the pair of guards `neon_resource_get` and `neon_resource_finish` branch
//    on.
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
// the shared tail. Whether its take succeeds is the observable consequence of get not
// disarming -- after a get with no take, the drop must still find the resource armed.
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

// One complete life: `takes` bare takes, then one `get`, then the last release. `takes` is
// a literal at each call site; see SCOPE note 3.
static void scenario(unsigned takes) {
    payload_drops = 0;
    env_drops = 0;
    cleanup_calls = 0;

    neon_header* g_payload = (neon_header*)neon_alloc(0, payload_drop);
    neon_header* g_env = (neon_header*)neon_alloc(0, env_drop);

    neon_closure cleanup;
    cleanup.fn = (void*)model_cleanup;
    cleanup.env = g_env; // the resource takes ownership of this reference

    neon_resource* r = neon_resource_new(&g_payload, &handle_witness, cleanup, model_drop);

    // A reference the harness keeps and releases last, so an imbalance in the payload is
    // caught as a count rather than as a use-after-free: `rc == 0` at the end is one
    // release too many, `rc == 2` is a leak.
    neon_retain(g_payload);
    PROVE(g_payload->rc == 2,
          "the payload's reference moved into the resource; the other is the harness pin");

    // The reference `get` will consume. The harness keeps one of its own so the resource
    // survives the call and its state can be inspected afterwards -- see SCOPE note 1,
    // which is where the header comment and the implementation disagree.
    neon_retain((neon_header*)r);
    PROVE(r->header.rc == 2, "the harness holds a reference beyond the one get will take");
    PROVE(r->armed, "a fresh resource is armed");

    bool expect_armed = true;

    // A bare take, when there is one, moves the payload out and consumes no reference --
    // it is how this model reaches the state where get must report the resource dead.
    if (takes >= 1) {
        neon_header* taken = UNWRITTEN;
        if (neon_resource_take(r, &taken)) {
            expect_armed = false;
            // We own the payload now and owe it a cleanup, exactly as emitted code does.
            ((cleanup_fn)r->cleanup.fn)(r->cleanup.env, taken);
        }
    }

    // ---- the get ----

    uint64_t payload_rc_before = g_payload->rc;
    neon_header* got = UNWRITTEN;
    bool live = neon_resource_get(r, &got);

    PROVE(live == expect_armed,
          "get reports liveness from the armed flag: true exactly while the payload is "
          "still the resource's to give");
    PROVE(r->header.rc == 1,
          "get consumes one reference to the resource -- the implementation is the truth "
          "here, not the 'without consuming' in neon/resource.h:49");
    PROVE(r->armed == expect_armed,
          "get does NOT disarm: a read never claims the cleanup out from under the drop");

    if (live) {
        PROVE(got == g_payload, "get yields the payload that went in");
        PROVE(g_payload->rc == payload_rc_before + 1,
              "get hands back an OWNED reference: it retains the payload through the "
              "witness before returning it, so the caller owes exactly one release");
        PROVE(*(neon_header**)neon_resource_payload(r) == g_payload,
              "and leaves the resource's own copy in place -- get reads, it does not move");
        neon_release(got); // the caller's owed release
        PROVE(g_payload->rc == payload_rc_before,
              "and giving that reference back returns the count to where it was: get is "
              "balanced against one release, not a leak and not a borrow");
    } else {
        PROVE(got == UNWRITTEN,
              "get on a resource whose payload has gone does not write out at all, so a "
              "use-after-release is a diagnosable false rather than a stale handle");
        PROVE(g_payload->rc == payload_rc_before,
              "and retains nothing when it reports the resource dead");
    }

    // ---- the last release: the drop runs, and finds whatever get left behind ----

    neon_release((neon_header*)r);

    PROVE(cleanup_calls == 1,
          "cleanup ran exactly once: the drop still found the resource armed when only a "
          "get had happened, and found it disarmed when a take had already won");
    PROVE(env_drops == 1, "the closure environment is released exactly once");
    PROVE(g_payload->rc == 1,
          "the payload was released exactly once, by whoever ran the cleanup -- get's "
          "owned reference was given back and released nothing extra");
    PROVE(payload_drops == 0, "the payload is not dropped while the harness still holds it");

    neon_release(g_payload);
    PROVE(payload_drops == 1, "the payload is dropped exactly once, and only at rc == 0");

    // Nothing else is freed by hand. The resource and the environment must both have been
    // reclaimed by the code under test; --memory-leak-check is the assertion.
}

int main(void) {
    scenario(0); // get on a live resource: an owned payload, and the resource still armed
    scenario(1); // a take moved the payload out first: get must report dead and write nothing
    return 0;
}
