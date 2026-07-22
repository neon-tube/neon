// Model: box a payload into an `any`, read it back through the accessors, then drop it.
//
// THE INVARIANT: `neon_box_new` copies the payload bytes verbatim (sized by the witness),
// stores the witness and the type tag, and publishes `rc == 1`; `neon_box_tag` and
// `neon_box_payload` read those back; and dropping the box releases the payload's counted
// contents EXACTLY ONCE through the witness, then frees the box itself.
//
// `any` is the one erasure boundary in the language, so this is the single place a value
// of any type is copied into a heap cell by raw `memcpy` and later released by a witness
// the box carried rather than one codegen knew statically. Two things can go wrong and
// neither is visible at the boxing site: the `memcpy` is sized by `w->size` read back out
// of the witness, so a wrong size tears the payload; and the box's `drop` reaches the
// payload's `release` through `b->w->release`, so a drop that skips it leaks and one that
// runs it twice double-frees. Both are ownership bugs a scalar payload hides -- a torn or
// double-released `int` is a silent no-op -- so the payload here is COUNTED (rule 7): a
// `neon_header*` with a real retain/release witness, carrying a marker word beside it so a
// torn copy is a failed assertion rather than a coincidentally-correct read.
//
// The reference is MOVED, not retained: `neon_box_new` copies the payload's bytes without
// touching its count, exactly as codegen hands a value it already owns across the erasure.
// The harness pins a second reference so the final balance is a count -- `rc == 0` is one
// release too many, `rc == 2` one too few -- caught at the imbalance, not at a later use.
//
// Verifies `src/any.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Baseline: 308 properties, VERIFICATION SUCCESSFUL. Six mutations, each reverted after.
//
// 1. `neon_box_new` corrupting the copied pointer (`->obj = NULL` written into the cell
//    after the `memcpy`) -- the concrete stand-in for a torn copy. Failed 5 of 320 on "the
//    payload pointer survives the copy", "the box holds its own copy", "dropping the box
//    releases the payload exactly once" and "the payload outlives its last release exactly
//    once" (the box now holds no reference, so the pinned count never falls), plus a leak.
//
// 2. `neon_box_new` corrupting the copied marker (`->marker ^= 1`). Failed 2 of 320 on
//    "the payload's marker word survives the copy" and "the box holds its own copy" -- the
//    marker beside the pointer is what turns a wrong-offset or short copy into an assertion
//    rather than a coincidentally-correct pointer read.
//
// 3. `neon_box_new` retaining the payload (`w->retain(payload)` before the copy) -- the
//    "be safe, take a reference" instinct that turns a move into a leak. Failed 4 of 321
//    on "neon_box_new moves the payload's reference rather than retaining it" (rc was 3)
//    and "the payload outlives its last release exactly once", plus a leak: the pinned
//    count never returns to 1 and the object is never dropped.
//
// 4. `neon_box_drop` skipping `w->release`. Failed 3 of 277 on "dropping the box releases
//    the payload exactly once" and "the payload outlives its last release exactly once" --
//    the count stays at 2 -- plus the leak. This is the bug this half exists to catch.
//
// 5. `neon_box_drop` releasing twice. Failed 6 of 351: the payload reached rc 0 while the
//    harness still held its pin ("the payload is not dropped while the harness still holds
//    its pin"), so the harness's own later release is a use-after-free -- a deallocated-
//    object dereference on `obj->rc` and an unsigned underflow on `h->rc - 1`.
//
// 6. `neon_box_new` storing the wrong tag (`tag + 1`). Failed 2 of 309 on "the box stores
//    the type tag it was given" and "hands it back through neon_box_tag".
//
// NOT USED, and why: deleting the `memcpy` outright leaves the payload cell as
// unconstrained heap, so the moved-out pointer the witness `release` later dereferences is
// symbolic -- the drop recursion behind it re-expands to the full unwind depth and the run
// does not finish (the README's symbolic-free explosion). The two concrete corruptions
// above (M1, M2) cover the same torn-copy failure without provoking it.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. ONE COUNTED FIELD, ONE SIZE. `w->size` is read from the witness by the code under
//    test, so the sizing arithmetic behind both the `memcpy` and the `payload`/`tag`
//    offsets is exercised -- but at a single 16-byte payload. A payload spanning several
//    counted fields, or one larger than a machine word in a way that stresses the
//    `sizeof(neon_box) - sizeof(neon_header) + w->size` extent, is not covered.
//
// 2. THE SCALAR PAYLOAD IS NOT MODELLED. `neon_box_drop` guards its release with
//    `if (b->w->release)`, and a witness with a NULL `release` (a boxed `i64`) takes the
//    other arm. That arm frees the box and touches nothing counted, so it has no ownership
//    failure mode to catch; this model drives the counted arm, which does. The guard
//    itself -- that a NULL `release` is not called -- is left to `--pointer-check` on the
//    counted run, where `b->w->release` is non-NULL and so says nothing about the NULL case.
//
// 3. `is`/`as` ARE NOT HERE. This proves the tag is stored and reads back; it does not
//    model a downcast comparing that tag, which is codegen-emitted control flow, not a
//    runtime entry point.
//
// 4. Out-of-memory does not appear as a return: `neon_alloc` traps on a NULL `malloc`
//    rather than handing one back, so `neon_box_new` has no failure path. `--malloc-may-fail
//    --malloc-fail-null` buys the check that the trap terminates rather than running on
//    with a NULL box, which the `_exit` stub encodes.
//
// 5. SINGLE-THREADED, like every model here: the count is a plain `uint64_t`.

#include "../support/cbmc_support.h"
#include "libneon_rt.h"

#include <stdio.h>

// Rule 4. `neon_trap` calls `fflush`/`fprintf`, `neon_alloc`'s out-of-memory check reaches
// a trap under `--malloc-fail-null`, and CBMC's models of those pull a `FILE` into that
// site. The model has nothing to say about stdio.
int fprintf(FILE* stream, const char* fmt, ...) { (void)stream; (void)fmt; return 0; }
int fflush(FILE* stream) { (void)stream; return 0; }

// ---- a counted payload, and the witness codegen would emit for it ----

static unsigned payload_drops;

static void payload_drop(void* p) {
    payload_drops++;
    neon_free(p);
}

// 16 bytes, deliberately not a single machine word: `marker` sits beside the counted
// pointer so a copy that lands at the wrong offset, or stops short, shows up as a corrupt
// marker rather than a coincidentally-correct pointer read.
typedef struct {
    uint64_t marker;
    neon_header* obj;
} payload_t;

#define MARKER 0xA51A51A5C0FFEE00ULL
#define TAG    0x0123456789ABCDEFULL

// The payload's value-witness: `size` is the whole struct, and retain/release forward to
// the lifecycle -- the shape codegen emits for an `any` erasing a type that owns a counted
// field. `neon_box_drop` reaches `release` through the stored `w`, which is the indirect
// call this model is built to exercise.
static void pl_retain(void* e)  { neon_retain(((payload_t*)e)->obj); }
static void pl_release(void* e) { neon_release(((payload_t*)e)->obj); }
static bool pl_eq(const void* a, const void* b) {
    return ((const payload_t*)a)->obj == ((const payload_t*)b)->obj;
}

static const neon_witness PL_W = {
    .size = sizeof(payload_t),
    .retain = pl_retain,
    .release = pl_release,
    .eq = pl_eq,
    .cmp = NULL,
};

int main(void) {
    // The counted object whose single reference will move into the box.
    neon_header* obj = (neon_header*)neon_alloc(0, payload_drop);
    PROVE(obj->rc == 1, "a fresh allocation is uniquely owned");

    // The reference the harness keeps, so the final balance is a count and not a
    // use-after-free: rc == 2 after boxing means one owner is the box, one is this pin.
    neon_retain(obj);
    PROVE(obj->rc == 2, "the harness pin and the reference about to move into the box");

    payload_t src = { MARKER, obj };
    neon_value v = neon_box_new(&src, &PL_W, TAG);
    neon_box* b = (neon_box*)v;

    PROVE(b->header.rc == 1, "a fresh box is uniquely owned: rc is 1");
    PROVE(b->w == &PL_W, "the box stores the witness it was given");
    PROVE(b->type_tag == TAG, "the box stores the type tag it was given");
    PROVE(neon_box_tag(v) == TAG, "and hands it back through neon_box_tag");

    payload_t* stored = (payload_t*)neon_box_payload(v);
    PROVE(stored->marker == MARKER, "the payload's marker word survives the copy");
    PROVE(stored->obj == obj, "the payload pointer survives the copy");
    PROVE(obj->rc == 2,
          "neon_box_new moves the payload's reference rather than retaining it: the count "
          "is unchanged, so the box now holds the reference the source slot did");

    // A copy, not an alias: scribbling the source slot must not reach into the box.
    src.marker = 0;
    src.obj = NULL;
    PROVE(stored->marker == MARKER && stored->obj == obj,
          "the box holds its own copy of the payload, not a view of the source slot");

    // ---- the drop: the box releases the payload through the witness, exactly once ----
    neon_release((neon_header*)v);
    PROVE(obj->rc == 1,
          "dropping the box releases the payload exactly once, through b->w->release");
    PROVE(payload_drops == 0,
          "the payload is not dropped while the harness still holds its pin");

    neon_release(obj);
    PROVE(payload_drops == 1,
          "the payload outlives its last release exactly once: dropped only at rc == 0");

    // The box itself was freed by neon_box_drop; nothing is reclaimed by hand here, so
    // --memory-leak-check is the assertion that it was.
    return 0;
}
