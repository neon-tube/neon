// Model: neon_list -- slot arithmetic, element ownership, and the COW boundary.
//
// Drives `src/list.c` -- the shipping source, compiled by CBMC alongside this
// harness, never a copy -- through push/at/set/concat/eq/cmp with a witness that
// has a real `retain`/`release`, so element ownership is observable.
//
// Why a witness with a release function: codegen hands elements to the list *by
// address*, and the list moves `w->size` bytes through that pointer. A wrong slot
// width is therefore a memory-safety bug, not a wrong answer -- the predecessor
// shipped a generic constructor emitting 24-byte slots that push/set read as 8,
// an ASan stack-buffer-overflow on every `list::new()`. That whole class of bug is
// invisible when `w->release` is NULL and elements are scalars, so the element
// here is a 16-byte struct carrying a self-checking payload and a per-identity
// ownership counter.
//
// ---- STATUS: this model currently FAILS, and the failure is real ----
//
// 26 of 1295 properties fail. Every one of them is the same defect on three
// lines, and no functional or ownership property fails:
//
//   list.c:58  neon_list_ensure_unique  memcpy(c->data, l->data, l->len * sz)
//   list.c:97  neon_list_concat         memcpy(r->data, a->data, a->len * sz)
//   list.c:98  neon_list_concat         memcpy(r->data + a->len * sz, b->data, ...)
//
// An empty list has `data == NULL`, so each of these calls `memcpy` with a NULL
// argument and a count of zero, and `concat` of two empty lists also forms
// `NULL + 0`. Both are undefined behaviour (C17 7.24.1p2 requires valid pointers
// regardless of the count; `memcpy` also carries `__attribute__((nonnull))`, from
// which GCC and Clang are entitled to infer the pointers are non-NULL and delete
// later checks). It is reached by ordinary programs -- `xs ++ []`, or a push to a
// shared empty list -- not just by an edge case a fuzzer would have to find.
//
// This is left failing on purpose. Weakening the model to go green would turn a
// found bug into the appearance of evidence. Fixing `list.c` is out of scope here.
//
// ---- what is proved (all of the below pass) ----
//
// For every length up to the bounds below, and on every malloc-failure branch
// `--malloc-fail-null` opens:
//
//   - `len <= cap` holds after every push, and `len` is exact across
//     push / set / concat;
//   - every element's *bytes* survive growth: after the pushes that reallocate,
//     slot i still holds the payload written into slot i, at byte offset
//     `i * w->size`;
//   - `neon_list_at` returns exactly `data + i * w->size`, in bounds, and an
//     out-of-range or negative index traps rather than returning a slot past
//     `len`;
//   - the witness's retain/release run the exact number of times required: a list
//     of counted elements neither leaks an element nor releases one twice
//     (`elem_release` proves the count is positive before decrementing, so a
//     double free fails there rather than being absorbed);
//   - copy-on-write is sound for both mutators: pushing to or setting into a
//     shared list (rc > 1) leaves the other reference's elements, length and
//     buffer untouched, and the copy retains each shared element for itself;
//   - no leak and no double free of the list itself on any path --
//     `--memory-leak-check` covers the header+body allocation and `data` together.
//
// ---- bounds, and what they do and do not cover ----
//
//   MAXN     5  pushes into one list. cap goes 0 -> 4 -> 8, so 5 is the least
//               bound that forces a *reallocating* push at all -- the case where
//               "an element's bytes survive a push" has any content. Does not
//               cover repeated doubling: only the first growth step ever runs.
//   SMALLN   3  indexing, set, and their lists. Growth is irrelevant to both.
//   CAT_MAX  2  concat, enumerated as the full 3x3 cross product of lengths.
//   COW_MAX  3  shared-list length, crossed with both mutators (push and set).
//   CMP_MAX  3  eq/cmp operand lengths.
//   CAP_MAX  3  requested capacity, enumerated concretely.
//
// No bound reaches a `cap` large enough for `cap * 2` or `ncap * sz` to approach
// `size_t` overflow, so the overflow checks say nothing about huge lists; one
// cannot be built inside a model. Every runtime loop reached here trips at most
// MAXSLOTS = 6 times, well under `--unwind 12`, with `--unwinding-assertions` on
// so that guessing a bound too low would fail rather than silently prove less.
//
// ---- what is deliberately NOT proved ----
//
//   - `neon_list_len`'s consuming release beyond one balanced use; it is two lines
//     whose only interesting behaviour is a release, covered by the lifecycle
//     model.
//   - `neon_list_cmp`/`neon_list_eq` are exercised for agreement and for slot
//     addressing, not for the full order law (transitivity across three lists);
//     that is a property of the element `cmp`, which codegen supplies. The lists
//     compared here share a common prefix, so it is cmp's length tiebreak under
//     test, not element disagreement.
//   - Aliasing between the two arguments of `concat`/`eq`/`cmp`. Codegen never
//     emits `concat(a, a)` -- and it would double-release -- so the model builds
//     two distinct lists.
//   - Concurrency: the runtime is single-threaded and `rc` is a plain `uint64_t`.
//   - The out-of-memory path *inside a trap*. `neon_list_push` assigns the
//     `realloc` result over `l->data` before checking it for NULL, so the old
//     buffer is unreachable by the time it calls `neon_trap`. That is a leak in
//     the strict sense, but `neon_trap` ends in `_exit` and the OS reclaims, so
//     the support header's `_exit` stub makes the continuation infeasible and the
//     leak unobservable here. Stated so the absence of a report is not mistaken
//     for the absence of the pointer store.
//
// ---- three shape requirements CBMC imposes, none of which is an assumption ----
//
// Each of these changes how the harness is written, not which states it explores.
// They are recorded because every one of them cost an afternoon to find.
//
// 1. Every bounded loop is written `for (i = 0; i < <constant>; i++) { if (i >= n)
//    break; ... }` rather than `i < n`. With a symbolic guard CBMC unwinds to the
//    full `--unwind` bound and duplicates every allocation site in the body that
//    many times; with a constant guard it unwinds exactly that many times. Written
//    the obvious way this model exhausts the solver's memory instead of finishing.
//    The `break` keeps the semantics identical.
//
// 2. `concat`, `ensure_unique` and `new_with_capacity` size a `memcpy` or a
//    `malloc` by `len * sz`. CBMC's built-in `memcpy` is imprecise when the byte
//    count is symbolic -- it leaves the copied bytes unconstrained, and every
//    downstream property then fails spuriously. (Not specific to Neon; it
//    reproduces in twenty lines of plain C.) A symbolic `malloc` size is sound but
//    exhausts the solver. So the scenarios reaching those functions are entered
//    with *concrete* lengths, enumerated by the switches near the bottom of this
//    file. Nothing within a bound is skipped -- the enumerations are exhaustive --
//    but coverage is by enumeration rather than by symbolic length, so raising a
//    bound means adding arms.
//
// 3. `fprintf`/`fflush` are stubbed below, and elements are staged through one
//    shared slot rather than a local per call site. Both are object-count
//    economies; see the comments at each.

#include "../support/cbmc_support.h"

#include <stdio.h>

#include "libneon_rt.h"

// Stub out the trap's I/O, for the same reason the support header stubs `_exit`.
// `neon_trap` calls `fflush`/`fprintf`, and CBMC's models of those pull a `FILE`
// and its buffers in at *every* trap site -- and this model reaches a trap from
// every allocation check, every out-of-range index, and every OOM branch that
// `--malloc-fail-null` opens. Left alone they account for most of the program's
// addressed objects and put it over CBMC's default `--object-bits 8`, which the
// shared CMake target does not override. Nothing under test is lost: what a trap
// prints is not a property of the list, and the trap still terminates the path
// via `_exit`.
int fprintf(FILE* stream, const char* fmt, ...) { (void)stream; (void)fmt; return 0; }
int fflush(FILE* stream) { (void)stream; return 0; }

// Not in the support header's nondet family, and indexing is the one place this
// model needs a *signed* 64-bit unconstrained value (a negative index must trap).
int64_t nondet_int64(void);

// ---- the element type and its witness ----

// 16 bytes, deliberately not a machine word: a slot-width bug then shows up as a
// payload landing at the wrong offset rather than as a coincidentally-correct
// read. `tag` is derived from `id`, so a torn or half-copied element is
// detectable and not just a swapped one.
typedef struct {
    uint64_t id;
    uint64_t tag;
} elem;

#define ELEM_TAG(i) (0xA51A51A500000000ULL ^ (uint64_t)(i))

// Distinct element identities the model can name, and the widest list it builds.
#define NIDS 8
#define MAXSLOTS 6
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
    // The double-free check. If the list released a slot it did not own, or
    // released one twice, the count is already zero by the time we get here.
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
    sizeof(elem), elem_retain, elem_release, elem_eq, elem_cmp,
};

// ---- helpers ----

// One staging slot for every element handed to the list, rather than a local in
// each caller. CBMC counts *addressed* objects, and with a local here every one
// of the ~90 inlined `push_owned` sites became a separate object -- enough on its
// own to exceed the default `--object-bits`. The list copies the bytes out of
// whatever address it is given, so a single reused slot exercises exactly the
// same code; what it does not model is two elements being live in distinct stack
// slots at once, which no list operation can observe.
static elem staging;

// Push element `id`, modelling codegen's move: the caller owns one reference and
// hands it to the list, so the harness's count rises by one and the list must be
// the thing that eventually releases it.
static neon_list* push_owned(neon_list* l, uint64_t id) {
    staging.id = id;
    staging.tag = ELEM_TAG(id);
    live[id]++;
    return neon_list_push(l, &staging);
}

// Build a fresh list holding identities `first .. first + n - 1`. `limit` is the
// caller's compile-time upper bound on `n` and is the loop's actual trip count
// (see shape requirement 1) -- passing a tighter one keeps the unrolled program
// smaller.
#define BUILD_RUN(dest, first, n, limit)                                       \
    do {                                                                       \
        (dest) = neon_list_new(&ELEM_W);                                       \
        for (unsigned _i = 0; _i < (limit); _i++) {                            \
            if (_i >= (n)) break;                                              \
            (dest) = push_owned((dest), (first) + _i);                         \
        }                                                                      \
    } while (0)

// The list holds exactly `n` elements and slot i holds identity `first + i` --
// read back through the public accessor, so this covers `neon_list_at`'s own
// address computation and not merely the raw buffer.
static void check_contents(neon_list* l, unsigned first, unsigned n) {
    PROVE(l->len == n, "the list's length is exactly the number of elements pushed");
    PROVE(l->len <= l->cap, "len <= cap");
    for (unsigned i = 0; i < MAXSLOTS; i++) {
        if (i >= n) break;
        elem* s = (elem*)neon_list_at(l, (int64_t)i);
        PROVE(s == (elem*)(l->data + (size_t)i * ELEM_W.size),
              "at(i) addresses data + i * w->size");
        PROVE(s->id == first + i, "element i is still in slot i");
        PROVE(s->tag == ELEM_TAG(first + i), "element i's bytes are intact");
    }
}

static void check_all_released(void) {
    for (unsigned i = 0; i < NIDS; i++) {
        PROVE(live[i] == 0, "dropping the list releases every element exactly once");
    }
}

// ---- scenarios ----
//
// Loop bound: at most MAXN pushes into one list. `neon_list_new` starts at cap 0,
// the first push takes cap to 4, and the fifth forces the `realloc` growth to 8 --
// so 5 is the smallest bound that exercises a *reallocating* push at all, which is
// the case where "an element's bytes survive a push" has any content. It does NOT
// cover repeated doublings (only the first growth step runs), nor any `cap` large
// enough for `cap * 2` or `ncap * sz` to approach `size_t` overflow. Every runtime
// loop reached here trips at most MAXSLOTS = 6 times, comfortably under
// `--unwind 12` with `--unwinding-assertions` left on.
#define MAXN 5
// The bound for scenarios where growth is irrelevant -- indexing, set, compare.
// Keeping those small is what fits the whole model inside CBMC's default
// `--object-bits 8` (256 addressed objects), which the shared CMake target does
// not override.
#define SMALLN 3
#define CMP_MAX 3
#define CAP_MAX 3
#define GROWTH_REASON                                                          \
    "element count; 5 is the least that forces the realloc growth step "       \
    "(cap 0 -> 4 -> 8), and larger counts only repeat the same doubling"

// 1. push: growth, byte survival, len/cap, and the full release on drop.
static void scenario_push(void) {
    unsigned n = NONDET_UPTO(MAXN, GROWTH_REASON);

    neon_list* l = neon_list_new(&ELEM_W);
    PROVE(l->len == 0 && l->cap == 0 && l->data == NULL,
          "a fresh list is empty with no buffer");

    for (unsigned i = 0; i < MAXN; i++) {
        if (i >= n) break;
        l = push_owned(l, i);
        PROVE(l->len == i + 1, "push increments len by exactly one");
        PROVE(l->len <= l->cap, "push maintains len <= cap");
    }
    PROVE(l->header.rc == 1, "an unshared list stays unshared across pushes");

    // Byte survival across every reallocation this run performed.
    check_contents(l, 0, n);

    for (unsigned i = 0; i < MAXN; i++) {
        if (i >= n) break;
        PROVE(live[i] == 1, "the list owns exactly one reference per element");
    }

    neon_release((neon_header*)l);
    check_all_released();
}

// 2. at: an out-of-range index must trap, never return a slot past len.
static void scenario_at_oob(void) {
    unsigned n = NONDET_UPTO(SMALLN,
        "list length; the trap is decided by `i < 0 || i >= len` alone, so no "
        "length beyond a couple reaches a different branch. Growth is "
        "scenario_push's job");
    ASSUME(n >= 1, "an empty list has no in-range index to contrast against; the "
                   "len == 0 list is built and indexed by scenario_concat");

    neon_list* l;
    BUILD_RUN(l, 0, n, SMALLN);

    int64_t i = nondet_int64();
    ASSUME(i < 0 || (uint64_t)i >= (uint64_t)l->len,
           "splits the index space; this arm is the out-of-range half, otherwise "
           "unconstrained so negative, == len and huge are all reachable. The "
           "in-range half is not excluded from the model -- check_contents reads "
           "every in-range index of every list built here");

    void* p = neon_list_at(l, i);
    (void)p;
    // Unreachable: `neon_list_at` traps and the support header's `_exit` stub
    // makes anything after a trap infeasible. If indexing ever returned a slot
    // instead, this fires.
    PROVE(0, "an out-of-range list index traps rather than returning a slot");
}

// 3. set on an unshared list: len unchanged, the displaced element released
//    exactly once, the new one installed at the right offset.
static void scenario_set(void) {
    unsigned n = NONDET_UPTO(SMALLN,
        "list length; set writes one slot of an already-built list and never "
        "grows, so the growth bound buys nothing here");
    ASSUME(n >= 1, "set needs an in-range index to exist; set's out-of-range "
                   "branch is the same trap scenario_at_oob covers");
    unsigned i = NONDET_UPTO(SMALLN, "the index written; every in-range slot of "
                                     "every reachable length is explored");
    ASSUME(i < n, "an in-range index -- the out-of-range branch traps, and is "
                  "covered by scenario_at_oob");

    neon_list* l;
    BUILD_RUN(l, 0, n, SMALLN);

    // A fresh identity, distinct from everything already in the list, so
    // displacing element i is distinguishable from overwriting it with itself.
    const uint64_t fresh = NIDS - 1;
    staging.id = fresh;
    staging.tag = ELEM_TAG(fresh);
    live[fresh]++;
    l = neon_list_set(l, (int64_t)i, &staging);

    PROVE(l->len == n, "set leaves len unchanged");
    PROVE(live[i] == 0, "set releases the displaced element exactly once");
    PROVE(live[fresh] == 1, "set takes ownership of the new element exactly once");
    elem* s = (elem*)neon_list_at(l, (int64_t)i);
    PROVE(s->id == fresh && s->tag == ELEM_TAG(fresh),
          "the new element's bytes land whole in slot i");
    for (unsigned k = 0; k < SMALLN; k++) {
        if (k >= n) break;
        if (k == i) continue;
        elem* o = (elem*)neon_list_at(l, (int64_t)k);
        PROVE(o->id == k && o->tag == ELEM_TAG(k), "set leaves every other slot alone");
        PROVE(live[k] == 1, "set does not touch any other element's refcount");
    }

    neon_release((neon_header*)l);
    check_all_released();
}

// 4. concat: exact length, order preserved across the seam, ownership
//    transferred exactly once. `n` and `m` are concrete -- see shape
//    requirement 2 -- and `main` enumerates every pair up to CAT_MAX.
#define CAT_MAX 2
static void scenario_concat(unsigned n, unsigned m) {
    neon_list* a;
    neon_list* b;
    BUILD_RUN(a, 0, n, n);
    BUILD_RUN(b, n, m, m);

    neon_list* r = neon_list_concat(a, b); // consumes both

    PROVE(r->len == (size_t)n + m, "concat's length is the exact sum of the two");
    PROVE(r->len <= r->cap, "concat maintains len <= cap");
    // Identities were handed out 0..n-1 then n..n+m-1, so the concatenation must
    // read back as the single run 0..n+m-1 -- order preserved across the seam.
    check_contents(r, 0, n + m);
    for (unsigned k = 0; k < MAXSLOTS; k++) {
        if (k >= n + m) break;
        PROVE(live[k] == 1, "concat leaves exactly one owned reference per element");
    }

    neon_release((neon_header*)r);
    check_all_released();
}

// 5. copy-on-write: mutating a shared list must not disturb the other holder.
//    Run for both mutators -- push and set both route through
//    `neon_list_ensure_unique`, and a miscounted retain there is exactly the bug
//    a scalar element type cannot see. `n` is concrete; see shape requirement 2.
#define COW_MAX 3
static void scenario_shared_cow(unsigned n, bool via_set) {
    neon_list* l;
    BUILD_RUN(l, 0, n, n);
    if (via_set && n == 0) {
        // No in-range index to set; nothing to do but tidy up.
        neon_release((neon_header*)l);
        check_all_released();
        return;
    }

    // A second holder. The mutator now consumes one of the two references and
    // must copy rather than mutate in place.
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
        mut = neon_list_set(l, 0, &staging); // slot 0 exists: n >= 1 here
    } else {
        mut = push_owned(l, n);
    }

    PROVE(mut != keep, "mutating a shared list copies instead of mutating in place");
    PROVE(keep->len == old_len, "the other holder's length is unchanged");
    PROVE(keep->data == old_data, "the other holder's buffer is neither moved nor freed");
    PROVE(keep->header.rc == 1, "the copy released the original exactly once");
    PROVE(mut->len <= mut->cap, "the copy maintains len <= cap");

    // The original is untouched whichever mutator ran.
    check_contents(keep, 0, n);

    if (via_set) {
        PROVE(mut->len == old_len, "set on a shared list does not change the copy's len");
        elem* s = (elem*)neon_list_at(mut, 0);
        PROVE(s->id == fresh, "the copy's slot 0 holds the new element");
        PROVE(live[0] == 1, "the displaced element is still owned by the original only");
        PROVE(live[fresh] == 1, "the new element is owned exactly once");
        for (unsigned k = 1; k < n; k++) {
            if (k >= n) break;
            PROVE(live[k] == 2, "the copy retained each shared element for itself");
        }
    } else {
        PROVE(mut->len == old_len + 1, "the copy carries the pushed element");
        check_contents(mut, 0, n + 1);
        for (unsigned k = 0; k < n; k++) {
            if (k >= n) break;
            PROVE(live[k] == 2, "the copy retained each shared element for itself");
        }
        PROVE(live[n] == 1, "the pushed element is owned exactly once");
    }

    neon_release((neon_header*)mut);
    for (unsigned k = 0; k < n; k++) {
        if (k >= n) break;
        PROVE(live[k] == 1, "dropping the copy leaves the original's references intact");
    }
    neon_release((neon_header*)keep);
    check_all_released();
}

// 6. eq / cmp: agreement, and the same slot arithmetic under a read-only walk.
static void scenario_eq_cmp(void) {
    unsigned n = NONDET_UPTO(CMP_MAX, "left length; 2 is the least that walks the "
                                      "comparison loop more than once, and every "
                                      "length up to it is explored");
    unsigned m = NONDET_UPTO(CMP_MAX, "right length; same bound, and n != m reaches "
                                      "the prefix case where length alone decides");

    // Both lists hold identities 0.., so one is always a prefix of the other and
    // the comparison is decided by length -- the case where cmp's final tiebreak
    // is load-bearing. Element disagreement is elem_cmp's business, not the list's.
    neon_list* a;
    neon_list* b;
    BUILD_RUN(a, 0, n, CMP_MAX);
    BUILD_RUN(b, 0, m, CMP_MAX);

    bool eq = neon_list_eq(a, b);
    int c = neon_list_cmp(a, b);
    PROVE(eq == (n == m), "lists over a common prefix are equal iff same length");
    PROVE(eq == (c == 0), "eq agrees with cmp == 0");
    PROVE(c == (n < m ? -1 : (n > m ? 1 : 0)),
          "a proper prefix sorts before the longer list");
    PROVE(neon_list_cmp(b, a) == -c, "cmp is antisymmetric on these lists");
    PROVE(a->header.rc == 1 && b->header.rc == 1, "cmp and eq borrow, never consume");

    neon_release((neon_header*)a);
    neon_release((neon_header*)b);
    check_all_released();
}

// 7. new_with_capacity, then the consuming len().
static void scenario_capacity(unsigned c) {
    neon_list* l = neon_list_new_with_capacity(&ELEM_W, (int64_t)c);
    PROVE(l->len == 0, "a list built with capacity is still empty");
    PROVE(l->cap == c, "capacity is exactly what was requested");

    unsigned n = NONDET_UPTO(CAP_MAX, "pushes into a preallocated buffer; the "
                                      "enumeration of c straddles this in both "
                                      "directions, which is what this scenario is "
                                      "for. Growth itself is scenario_push's job");
    for (unsigned i = 0; i < CAP_MAX; i++) {
        if (i >= n) break;
        l = push_owned(l, i);
        PROVE(l->len <= l->cap, "len <= cap holds with a preallocated buffer too");
    }
    check_contents(l, 0, n);

    // `neon_list_len` consumes its argument, so retain first to keep the list.
    neon_retain((neon_header*)l);
    int64_t got = neon_list_len(l);
    PROVE(got == (int64_t)n, "len() reports the exact number of elements");
    PROVE(l->header.rc == 1, "len() releases the reference it consumed");

    neon_release((neon_header*)l);
    check_all_released();
}

// Exhaustive enumeration of the concrete-length arms. Each `case` is a distinct
// concrete length, so the `memcpy` byte counts inside `concat` and
// `ensure_unique` are constants; the set of cases is the full cross product up to
// the bound, so nothing in range is skipped.
static void dispatch_concat(void) {
    unsigned k = NONDET_UPTO((CAT_MAX + 1) * (CAT_MAX + 1) - 1,
        "selects one of the nine (n, m) length pairs below. The enumeration is "
        "the full cross product of 0..CAT_MAX on both sides, so no length pair "
        "within the bound is skipped. CAT_MAX is 2 because what concat's code "
        "distinguishes is empty / one / more-than-one on each side -- the copy "
        "and retain loops are otherwise length-generic");
    switch (k) {
        case 0: scenario_concat(0, 0); break;
        case 1: scenario_concat(0, 1); break;
        case 2: scenario_concat(0, 2); break;
        case 3: scenario_concat(1, 0); break;
        case 4: scenario_concat(1, 1); break;
        case 5: scenario_concat(1, 2); break;
        case 6: scenario_concat(2, 0); break;
        case 7: scenario_concat(2, 1); break;
        default: scenario_concat(2, 2); break;
    }
}

static void dispatch_capacity(void) {
    unsigned c = NONDET_UPTO(CAP_MAX,
        "requested capacity, enumerated concretely so malloc's size is a constant "
        "(shape requirement 2 applies to sized allocation too -- a symbolic "
        "malloc size exhausted the solver). 0 must leave data NULL, and 1..CAP_MAX "
        "sit both below and above the number of pushes that follow");
    switch (c) {
        case 0: scenario_capacity(0); break;  // no buffer at all
        case 1: scenario_capacity(1); break;  // a buffer the pushes overrun
        case 2: scenario_capacity(2); break;  // a buffer the pushes exactly fill
        default: scenario_capacity(3); break; // a buffer the pushes never fill
    }
}

static void dispatch_cow(void) {
    unsigned k = NONDET_UPTO(2 * (COW_MAX + 1) - 1,
        "selects a shared-list length 0..COW_MAX crossed with the two mutators "
        "(push and set); exhaustive over both within the bound. COW_MAX is 2 "
        "because 2 is already more than one element in ensure_unique's "
        "copy-and-retain loop, and 3 additionally makes the copy's buffer exactly "
        "full. It does not reach a shared list whose copy then grows again");
    bool via_set = (k & 1u) != 0;
    switch (k >> 1) {
        case 0: scenario_shared_cow(0, via_set); break;
        case 1: scenario_shared_cow(1, via_set); break;
        case 2: scenario_shared_cow(2, via_set); break;
        default: scenario_shared_cow(3, via_set); break;
    }
}

int main(void) {
    unsigned scenario = NONDET_UPTO(6,
        "harness dispatch only -- it selects which scenario runs and constrains "
        "no input the runtime sees. Every arm is verified in full");

    switch (scenario) {
        case 0: scenario_push(); break;
        case 1: scenario_at_oob(); break;
        case 2: scenario_set(); break;
        case 3: dispatch_concat(); break;
        case 4: dispatch_cow(); break;
        case 5: scenario_eq_cmp(); break;
        default: dispatch_capacity(); break;
    }
    return 0;
}
