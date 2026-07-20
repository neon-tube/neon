// Model: pushes into a fresh list, across the reallocating growth step, reading every
// slot back through the public accessor.
//
// THE INVARIANT: `neon_list_push` increments `len` by exactly one, maintains
// `len <= cap`, and every element's bytes survive the reallocating growth step at their
// correct offset.
//
// The growth step is the part worth a machine check. `neon_list_new` starts at `cap 0`;
// the first push takes it to 4 and the fifth `realloc`s to 8, and that `realloc` sizes
// itself as `ncap * sz` while the copy that preceded it was addressed as `len * sz`. If
// those two ever disagreed about the slot width, elements would land at the wrong offset
// in the new buffer -- and that is not a wrong answer, it is a memory-safety bug, because
// codegen hands elements to the list *by address* and the list moves `w->size` bytes
// through that pointer. The predecessor project shipped a generic constructor emitting
// 24-byte slots that push and set read as 8: an ASan stack-buffer-overflow on every
// `list::new()`. That whole class of bug is invisible when `w->release` is NULL and the
// element is a scalar, so the element here is a 16-byte struct whose `tag` is derived
// from its `id` -- a torn or half-copied element is then detectable, not merely a
// swapped one -- carrying a per-identity ownership counter (rule 7).
//
// The counter is also what makes "grows without losing bytes" mean "and without losing
// ownership": the list must hold exactly one reference per element after the growth, and
// dropping it must return every one of them.
//
// Verifies `src/list.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Baseline: 831 properties, VERIFICATION SUCCESSFUL. Three mutations, each reverted.
//
// 1. `neon_list_push`'s copy widened with a literal: `memcpy(l->data + l->len * sz, elem,
//    sz)` -> `... l->len * 8, elem, 8`. This is the predecessor's shipped bug, and the one
//    the 16-byte self-checking element exists for: a 16-byte element written at an 8-byte
//    stride puts every slot but the first over its neighbour, and the tag no longer
//    derives from the id. Failed 9 of 829 -- on "element i is still in slot i after the
//    growth step", "element i's bytes are intact after the growth step", "dropping the
//    list releases every element exactly once", and inside the witness on "release is
//    handed a well-formed element". Shipped, this is a heap overflow on every list of a
//    non-word element, which is what it was last time.
//
// 2. `l->cap = ncap` dropped after the `realloc`, so the buffer grows but the list never
//    learns it did. Failed 15 of 825, first on "push maintains len <= cap" -- the
//    invariant that says the next push writes inside the allocation -- then on element
//    identity and the drop accounting. Shipped: `len` runs past `cap`, every subsequent
//    push reallocs to the same size, and writes walk off the end.
//
// 3. `l->len++` written twice. Failed 10 of 838 on "push increments len by exactly one",
//    then on slot identity and the drop count -- the phantom slot is uninitialised memory
//    the drop hands to `release`.
//
// NOT caught by this model, and correctly so: `neon_list_new_with_capacity` allocating
// `cap` bytes instead of `cap * w->size` (verified SUCCESSFUL here, 824 properties). This
// model's lists start empty, so the preallocating path is never taken; that mutation is
// caught by list-new-with-capacity-preallocates. Scope, not blindness.
//
// ---- SCOPE: what this model does not cover ----
//
// 1. Only the FIRST growth step ever runs. MAXN is 5, which is the least bound that
//    forces a `realloc` at all (cap 0 -> 4 -> 8); repeated doubling, and any `cap` large
//    enough for `cap * 2` or `ncap * sz` to approach `size_t` overflow, are not reached.
//    Consequently NOT proved: that growth is correct at a size where the multiplication
//    overflow checks in `neon_list_push` have anything to say.
//
// 2. The list here is sole-owned throughout, so `neon_list_ensure_unique` returns
//    immediately and the copy-on-write path inside push is never taken. NOT proved here:
//    that a push to a shared list copies -- that is
//    `list-mutating-a-shared-list-copies-it`'s claim.
//
// 3. Pushing is exercised only with elements the harness owns and hands over. NOT
//    proved: any behaviour for a witness whose `retain`/`release` are NULL, which is the
//    scalar element repr codegen also emits.
//
// 4. The out-of-memory path *inside* the trap. `neon_list_push` assigns the `realloc`
//    result over `l->data` before checking it for NULL, so the old buffer is unreachable
//    by the time it calls `neon_trap`. That is a leak in the strict sense, but
//    `neon_trap` ends in `_exit` and the support header's stub makes the continuation
//    infeasible, so the leak is unobservable here. Stated so the absence of a report is
//    not mistaken for the absence of the pointer store.

#include "../support/cbmc_support.h"

#include <stdio.h>

#include "libneon_rt.h"

// Rule 4. `neon_trap` calls `fflush`/`fprintf`, and CBMC's models of those pull a `FILE`
// and its buffers in at *every* trap site -- and a trap is reachable here from every
// allocation check and every OOM branch `--malloc-fail-null` opens. Left alone they
// account for most of the program's addressed objects and put it over CBMC's default
// `--object-bits 8`, which the shared CMake target does not override. Nothing under test
// is lost: what a trap prints is not a property of the list.
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

// Distinct element identities the model can name, and the widest list it builds.
#define NIDS 8
#define MAXSLOTS 5
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
    // The over-release oracle: if the list released a slot it did not own, or released
    // one twice, the count is already zero by the time we get here -- so the failure
    // names the call in list.c that made it, not some later use.
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
// caller (shape requirement 3). CBMC counts *addressed* objects, and with a local here
// every inlined `push_owned` site becomes a separate object -- enough on its own to
// exceed the default `--object-bits`. The list copies the bytes out of whatever address
// it is given, so a single reused slot exercises exactly the same code; what it does not
// model is two elements being live in distinct stack slots at once, which no list
// operation can observe.
static elem staging;

// Push element `id`, modelling codegen's move: the caller owns one reference and hands
// it to the list, so the harness's count rises by one and the list must be the thing
// that eventually releases it.
static neon_list* push_owned(neon_list* l, uint64_t id) {
    staging.id = id;
    staging.tag = ELEM_TAG(id);
    live[id]++;
    return neon_list_push(l, &staging);
}

// 5 is the least bound that forces the realloc growth step (cap 0 -> 4 -> 8); larger
// counts only repeat the same doubling. Every runtime loop reached here trips at most
// MAXSLOTS = 5 times, comfortably under `--unwind 12` with `--unwinding-assertions` on.
#define MAXN 5

int main(void) {
    unsigned n = NONDET_UPTO(MAXN,
        "element count; 5 is the least that forces the realloc growth step "
        "(cap 0 -> 4 -> 8), and larger counts only repeat the same doubling");

    neon_list* l = neon_list_new(&ELEM_W);
    PROVE(l->len == 0 && l->cap == 0 && l->data == NULL,
          "a fresh list is empty with no buffer");

    for (unsigned i = 0; i < MAXN; i++) { // constant bound, rule 3
        if (i >= n) break;
        l = push_owned(l, i);
        PROVE(l->len == i + 1, "push increments len by exactly one");
        PROVE(l->len <= l->cap, "push maintains len <= cap");
    }
    PROVE(l->header.rc == 1, "an unshared list stays unshared across pushes");

    // Byte survival across every reallocation this run performed, read back through the
    // public accessor so `neon_list_at`'s own address computation is covered too and not
    // merely the raw buffer.
    for (unsigned i = 0; i < MAXSLOTS; i++) { // constant bound, rule 3
        if (i >= n) break;
        elem* s = (elem*)neon_list_at(l, (int64_t)i);
        PROVE(s == (elem*)(l->data + (size_t)i * ELEM_W.size),
              "at(i) addresses data + i * w->size");
        PROVE(s->id == i, "element i is still in slot i after the growth step");
        PROVE(s->tag == ELEM_TAG(i), "element i's bytes are intact after the growth step");
        PROVE(live[i] == 1, "the list owns exactly one reference per element");
    }

    neon_release((neon_header*)l);
    for (unsigned i = 0; i < NIDS; i++) { // constant bound, rule 3
        PROVE(live[i] == 0, "dropping the list releases every element exactly once");
    }
    return 0;
}
