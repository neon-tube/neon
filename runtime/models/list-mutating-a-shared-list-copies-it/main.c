// Model: takes a second reference to a built list, mutates through the first, and then
// audits the second holder's length, buffer, elements and refcounts.
//
// THE INVARIANT: mutating a list whose `rc > 1` copies instead of mutating in place --
// the other holder's length, buffer and elements are untouched, and the copy retains
// each shared element for itself. Driven through both mutators, `push` and `set`.
//
// This is the property the whole refcount discipline rests on, and a violation of it is
// silent. `neon_list_ensure_unique` is the only thing standing between "two names for a
// value" and "two names for a mutable buffer": if it returned `l` unconditionally, a
// `push` through one binding would append to a list the other holder is reading, with no
// crash and no output difference until that holder is next read. Both mutators route
// through it, which is why both are driven here -- `set` additionally writes over a slot
// the *original* still owns, so a copy that forgot to retain would hand `set`'s
// `w->release(slot)` an element the original is about to release again.
//
// The retain-per-shared-element half is exactly the bug a scalar element type cannot see
// (rule 7): with `w->retain` NULL the copy loop in `ensure_unique` is dead code, the
// counts stay balanced by accident, and the model proves nothing. The element here is a
// 16-byte struct with a real retain/release and a per-identity counter, so `live[k] == 2`
// after the copy is a checkable claim; `elem_release` asserts the count is positive
// before decrementing, so an over-release fails at the call in `list.c` that made it.
//
// The empty-list case matters and is covered by the `n == 0` arm on the push side. An
// empty list has `data == NULL`, and `ensure_unique`'s copy calls `memcpy` on it --
// `memcpy` requires valid pointers even for a count of zero (C17 7.24.1p2), and its
// `nonnull` attribute entitles GCC and Clang to infer the arguments are non-NULL and
// delete later checks, so this is exploitable UB rather than a technicality. `list.c`
// guards it with `if (l->len != 0)`; that guard was added because this model's
// predecessor found its absence, and the `n == 0` arm is what keeps it there. A push to
// a shared empty list is an ordinary program, not an edge case a fuzzer must find.
//
// Verifies `src/list.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Baseline: 995 properties, VERIFICATION SUCCESSFUL. Four mutations, each reverted.
//
// 1. `neon_list_ensure_unique` made to `return l` unconditionally -- copy-on-write that
//    never copies, so a mutation through one holder is visible through every other. Failed
//    11 of 858, the whole model at once: "mutating a shared list copies instead of
//    mutating in place", "the other holder's length is unchanged", "the other holder still
//    sees its own element in slot k", "with its bytes intact", "the copy is sole-owned, so
//    the next mutation is in place". This is the defect the model exists for and it is
//    caught on every claim, not one.
//
// 2. The per-element retain loop deleted, so the copy holds pointers to elements it never
//    took a reference to. Failed 6 of 915, on "the copy retained each shared element for
//    itself", "dropping the copy leaves the original's references intact", "dropping both
//    holders releases every element exactly once in total", and in the witness on "no
//    element is released more times than it was retained". Note the split from mutation 1:
//    the buffer copy is right, so only the ownership claims fire. Shipped, this is a
//    use-after-free the moment either holder is dropped -- silent until then.
//
// 3. `neon_release((neon_header*)l)` on the original dropped, so the copy never gives back
//    the reference it consumed. Failed 3 of 995 on "the copy released the original exactly
//    once", the total-release claim, and CBMC's own memory-leak check. A pure leak: every
//    mutation of a shared list strands the old buffer and its elements forever.
//
// 4. REGRESSION CHECK -- the defect the previous generation of this model found, restored:
//    the `if (l->len != 0)` guard at list.c:63 removed, so an empty list is copied with
//    `memcpy(c->data, NULL, 0)`. Failed 1 of 995, on CBMC's `memcpy` contract --
//    "precondition_instance: memcpy source region readable" at line 63. The fix stays
//    fixed. One property, and it is the only signal: no corpus program copies an empty
//    list, so nothing else in the tree would ever have told us.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. Lengths are enumerated concretely up to 3 because `ensure_unique` sizes a `memcpy`
//    and `new_with_capacity` a `malloc` by `len * sz`, and CBMC's built-in `memcpy` is
//    imprecise with a symbolic byte count -- it leaves the copied bytes unconstrained and
//    every property below fails spuriously (shape requirement 2; it reproduces in twenty
//    lines of plain C). The enumeration is exhaustive within the bound, but coverage is
//    BY ENUMERATION: raising it means adding arms, and lengths above 3 are NOT proved.
//    3 is the bound because 2 already runs the copy-and-retain loop more than once and 3
//    additionally makes the copy's buffer exactly full.
//
// 2. The copy's buffer is sized `len ? len : 1`, so it is full immediately and a
//    subsequent push would grow it. That second push is not performed. NOT proved: that
//    a copy-on-write result grows correctly afterwards.
//
// 3. `rc` reaches 2, never higher. NOT proved: that a third holder is also undisturbed --
//    though `ensure_unique` branches only on `rc == 1`, so 2 and 7 take the same path.
//
// 4. `neon_list_set_scalar_inplace` deliberately has NO uniqueness check; its
//    precondition is the optimiser's to keep, and violating it is exactly the bug this
//    model would otherwise catch. It is not driven here.
//
// 5. Concurrency: the runtime is single-threaded and `rc` is a plain `uint64_t`, so
//    "shared" here means two references in one thread.

#include "../support/cbmc_support.h"

#include <stdio.h>

#include "libneon_rt.h"

// Rule 4. `neon_trap` calls `fflush`/`fprintf`, and CBMC's models of those pull a `FILE`
// and its buffers in at *every* trap site -- reachable here from every allocation check
// in the copy path and every OOM branch `--malloc-fail-null` opens. Left alone they
// account for most of the program's addressed objects and put it over CBMC's default
// `--object-bits 8`, which the shared CMake target does not override. What a trap prints
// is not a property of the list.
int fprintf(FILE* stream, const char* fmt, ...) { (void)stream; (void)fmt; return 0; }
int fflush(FILE* stream) { (void)stream; return 0; }

// ---- the element type and its witness ----

// 16 bytes, deliberately not a machine word: a slot-width bug then shows up as a payload
// landing at the wrong offset rather than as a coincidentally-correct read. `tag` is
// derived from `id`, so a torn or half-copied element is detectable and not just a
// swapped one.
typedef struct {
    uint64_t id;
    uint64_t tag;
} elem;

#define ELEM_TAG(i) (0xA51A51A500000000ULL ^ (uint64_t)(i))

#define NIDS 8
#define COW_MAX 3
#define MAXSLOTS (COW_MAX + 1)
static int live[NIDS]; // net owned references per identity

static void elem_retain(void* p) {
    elem* e = (elem*)p;
    PROVE(e->id < NIDS, "retain is handed a well-formed element");
    PROVE(e->tag == ELEM_TAG(e->id), "retain is handed intact element bytes");
    live[e->id]++;
}

static void elem_release(void* p) {
    elem* e = (elem*)p;
    PROVE(e->id < NIDS, "release is handed a well-formed element");
    PROVE(e->tag == ELEM_TAG(e->id), "release is handed intact element bytes");
    // The over-release oracle: it fails at the call in list.c that released once too
    // often, not at some later use.
    PROVE(live[e->id] > 0, "no element is released more times than it was retained");
    live[e->id]--;
}

static bool elem_eq(const void* a, const void* b) {
    return ((const elem*)a)->id == ((const elem*)b)->id;
}

static int elem_cmp(const void* a, const void* b) {
    uint64_t x = ((const elem*)a)->id, y = ((const elem*)b)->id;
    return x < y ? -1 : (x > y ? 1 : 0);
}

static const neon_witness ELEM_W = {
    .size = sizeof(elem),
    .retain = elem_retain,
    .release = elem_release,
    .eq = elem_eq,
    .cmp = elem_cmp,
};

// One staging slot for every element handed to the list, rather than a local in each
// caller (shape requirement 3): CBMC counts addressed objects, and a local per inlined
// push/set site is enough on its own to exceed the default `--object-bits`.
static elem staging;

static neon_list* push_owned(neon_list* l, uint64_t id) {
    staging.id = id;
    staging.tag = ELEM_TAG(id);
    live[id]++;
    return neon_list_push(l, &staging);
}

// `n` arrives concrete from the enumeration in `main`; see SCOPE note 1. It is therefore
// also the loop's literal trip count.
static void shared_cow(unsigned n, bool via_set) {
    neon_list* l = neon_list_new(&ELEM_W);
    for (unsigned i = 0; i < n; i++) { // constant bound, rule 3: n is a literal here
        l = push_owned(l, i);
    }

    if (via_set && n == 0) {
        // No in-range index to set; `set` would trap, which is another model's claim.
        neon_release((neon_header*)l);
        return;
    }

    // A second holder. The mutator now consumes one of the two references and must copy
    // rather than mutate in place.
    neon_retain((neon_header*)l);
    neon_list* keep = l;
    char* old_data = l->data;
    size_t old_len = l->len;

    const uint64_t fresh = NIDS - 1;
    neon_list* mut;
    if (via_set) {
        staging.id = fresh;
        staging.tag = ELEM_TAG(fresh);
        live[fresh]++;
        mut = neon_list_set(l, 0, &staging); // slot 0 exists: n >= 1 on this path
    } else {
        mut = push_owned(l, n);
    }

    PROVE(mut != keep, "mutating a shared list copies instead of mutating in place");
    PROVE(keep->len == old_len, "the other holder's length is unchanged");
    PROVE(keep->data == old_data, "the other holder's buffer is neither moved nor freed");
    PROVE(keep->header.rc == 1, "the copy released the original exactly once");
    PROVE(mut->header.rc == 1, "the copy is sole-owned, so the next mutation is in place");
    PROVE(mut->len <= mut->cap, "the copy maintains len <= cap");

    // The original is untouched whichever mutator ran -- bytes as well as length.
    for (unsigned k = 0; k < MAXSLOTS; k++) { // constant bound, rule 3
        if (k >= n) break;
        elem* s = (elem*)neon_list_at(keep, (int64_t)k);
        PROVE(s->id == k, "the other holder still sees its own element in slot k");
        PROVE(s->tag == ELEM_TAG(k), "with its bytes intact");
    }

    if (via_set) {
        PROVE(mut->len == old_len, "set on a shared list does not change the copy's len");
        elem* s = (elem*)neon_list_at(mut, 0);
        PROVE(s->id == fresh && s->tag == ELEM_TAG(fresh),
              "the copy's slot 0 holds the new element, whole");
        PROVE(live[0] == 1,
              "the element set displaced is still owned by the original only");
        PROVE(live[fresh] == 1, "the new element is owned exactly once");
        for (unsigned k = 1; k < MAXSLOTS; k++) { // constant bound, rule 3
            if (k >= n) break;
            PROVE(live[k] == 2, "the copy retained each shared element for itself");
        }
    } else {
        PROVE(mut->len == old_len + 1, "the copy carries the pushed element");
        for (unsigned k = 0; k < MAXSLOTS; k++) { // constant bound, rule 3
            if (k >= n) break;
            elem* s = (elem*)neon_list_at(mut, (int64_t)k);
            PROVE(s->id == k && s->tag == ELEM_TAG(k),
                  "the copy carries each shared element at its own offset");
            PROVE(live[k] == 2, "the copy retained each shared element for itself");
        }
        elem* last = (elem*)neon_list_at(mut, (int64_t)n);
        PROVE(last->id == n && last->tag == ELEM_TAG(n),
              "and the pushed element lands in the slot past them");
        PROVE(live[n] == 1, "the pushed element is owned exactly once");
    }

    neon_release((neon_header*)mut);
    for (unsigned k = 0; k < MAXSLOTS; k++) { // constant bound, rule 3
        if (k >= n) break;
        PROVE(live[k] == 1, "dropping the copy leaves the original's references intact");
    }
    neon_release((neon_header*)keep);
    for (unsigned k = 0; k < NIDS; k++) { // constant bound, rule 3
        PROVE(live[k] == 0,
              "dropping both holders releases every element exactly once in total");
    }
}

int main(void) {
    // Exhaustive enumeration of a concrete shared-list length crossed with the two
    // mutators. Each arm passes `n` as a literal so the `memcpy` byte count and `malloc`
    // size inside `ensure_unique` are constants; see SCOPE note 1.
    unsigned k = NONDET_UPTO(2 * (COW_MAX + 1) - 1,
        "selects a shared-list length 0..COW_MAX crossed with the two mutators (push and "
        "set); harness dispatch only, it constrains no input the runtime sees");
    bool via_set = (k & 1u) != 0;
    switch (k >> 1) {
        case 0: shared_cow(0, via_set); break;  // shared empty list: data == NULL
        case 1: shared_cow(1, via_set); break;
        case 2: shared_cow(2, via_set); break;  // copy loop runs more than once
        default: shared_cow(3, via_set); break; // copy's buffer exactly full
    }
    return 0;
}
