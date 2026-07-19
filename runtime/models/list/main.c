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
// What is proved (for every reachable length up to the bound, and on every
// malloc-failure branch `--malloc-fail-null` opens):
//
//   - `len <= cap` holds after every push, and `len` is exact across
//     push / set / concat;
//   - every element's *bytes* survive growth: after the pushes that reallocate,
//     slot i still holds the payload written into slot i, at the byte offset
//     `i * w->size`;
//   - `neon_list_at` returns exactly `data + i * w->size`, in bounds, and an
//     out-of-range or negative index traps rather than returning a slot past
//     `len`;
//   - the witness's retain/release run the exact number of times required: a list
//     of counted elements neither leaks an element nor releases one twice
//     (`elem_release` proves the count is positive before decrementing, so a
//     double free fails here rather than being absorbed);
//   - copy-on-write is sound: mutating a shared list (rc > 1) leaves the other
//     reference's elements, length and buffer untouched, and the copy retains
//     each shared element for itself;
//   - no leak and no double free of the list itself on any path --
//     `--memory-leak-check` covers the header+body allocation and `data` together.
//
// What is deliberately NOT proved:
//
//   - Element counts above the bound. See the `MAXN` note below for exactly what
//     the bound does and does not reach.
//   - `neon_list_len`'s consuming release beyond one balanced use; it is two lines
//     whose only interesting behaviour is a release, covered by the lifecycle
//     model.
//   - `neon_list_cmp`/`neon_list_eq` are exercised for agreement and for slot
//     addressing, not for the full order law (transitivity across three lists);
//     that is a property of the element `cmp`, which codegen supplies.
//   - Concurrency: the runtime is single-threaded and `rc` is a plain `uint64_t`.
//   - Overflow of `cap * 2` or `ncap * sz` in growth. The bound here is a handful
//     of elements, so the overflow checks never see a large `cap`; a list big
//     enough to overflow `size_t` cannot be built inside a model.
//   - Aliasing between the two arguments of `concat`/`eq`/`cmp`. Codegen never
//     emits `concat(a, a)`, and it would double-release; the model builds two
//     distinct lists.
//   - The out-of-memory path *inside a trap*. `neon_list_push` assigns the
//     `realloc` result over `l->data` before checking it for NULL, so the old
//     buffer is unreachable by the time it calls `neon_trap`. That is a leak in
//     the strict sense, but `neon_trap` ends in `_exit` and the OS reclaims, so
//     the support header's `_exit` stub makes the continuation infeasible and the
//     leak unobservable here. Stated so the absence of a report is not mistaken
//     for the absence of the pointer store.
//
// A note on loop shape, because it is not cosmetic. Every bounded loop here is
// written `for (i = 0; i < <constant>; i++) { if (i >= n) break; ... }` rather
// than `i < n`. With a symbolic guard CBMC unwinds to the full `--unwind` bound
// and duplicates every allocation site inside the body that many times; with a
// constant guard it unwinds exactly that many times. Written the obvious way this
// model exhausts the SAT solver's memory rather than finishing. The `break` on the
// symbolic condition keeps the semantics identical.

#include "../support/cbmc_support.h"

#include "libneon_rt.h"

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

// Push element `id`, modelling codegen's move: the caller owns one reference and
// hands it to the list, so the harness's count rises by one and the list must be
// the thing that eventually releases it.
static neon_list* push_owned(neon_list* l, uint64_t id) {
    elem e;
    e.id = id;
    e.tag = ELEM_TAG(id);
    live[id]++;
    return neon_list_push(l, &e);
}

// Push identities `first .. first + n - 1` in order.
static neon_list* push_run(neon_list* l, unsigned first, unsigned n) {
    for (unsigned i = 0; i < MAXSLOTS; i++) {
        if (i >= n) break;
        l = push_owned(l, first + i);
    }
    return l;
}

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
// loop reached here trips at most MAXSLOTS = 6 times (concat's 3 + 3), comfortably
// under `--unwind 12` with `--unwinding-assertions` left on.
#define MAXN 5
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

    // Byte survival across every reallocation the run performed.
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
    unsigned n = NONDET_UPTO(MAXN, GROWTH_REASON);
    ASSUME(n >= 1, "an empty list has no in-range index to contrast against; the "
                   "len == 0 list is built and indexed by scenario_concat");

    neon_list* l = push_run(neon_list_new(&ELEM_W), 0, n);

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

// 3. set: len unchanged, the displaced element released exactly once, the new
//    one installed at the right offset.
static void scenario_set(void) {
    unsigned n = NONDET_UPTO(MAXN, GROWTH_REASON);
    ASSUME(n >= 1, "set needs an in-range index to exist; set's out-of-range "
                   "branch is the same trap scenario_at_oob covers");
    unsigned i = NONDET_UPTO(MAXN, "the index written; every in-range slot of "
                                   "every reachable length is explored");
    ASSUME(i < n, "an in-range index -- the out-of-range branch traps, and is "
                  "covered by scenario_at_oob");

    neon_list* l = push_run(neon_list_new(&ELEM_W), 0, n);

    // A fresh identity, distinct from everything already in the list, so
    // displacing element i is distinguishable from overwriting it with itself.
    const uint64_t fresh = NIDS - 1;
    elem e;
    e.id = fresh;
    e.tag = ELEM_TAG(fresh);
    live[fresh]++;
    l = neon_list_set(l, (int64_t)i, &e);

    PROVE(l->len == n, "set leaves len unchanged");
    PROVE(live[i] == 0, "set releases the displaced element exactly once");
    PROVE(live[fresh] == 1, "set takes ownership of the new element exactly once");
    elem* s = (elem*)neon_list_at(l, (int64_t)i);
    PROVE(s->id == fresh && s->tag == ELEM_TAG(fresh),
          "the new element's bytes land whole in slot i");
    for (unsigned k = 0; k < MAXN; k++) {
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
//    transferred exactly once.
static void scenario_concat(void) {
    unsigned n = NONDET_UPTO(3, "left length; 3 + 3 keeps concat's copy/retain "
                                "loop at 6 trips, under --unwind 12. Includes 0, "
                                "the empty-buffer memcpy case");
    unsigned m = NONDET_UPTO(3, "right length; same bound and reason, and n == m "
                                "== 0 reaches concat of two empty lists");

    neon_list* a = push_run(neon_list_new(&ELEM_W), 0, n);
    neon_list* b = push_run(neon_list_new(&ELEM_W), n, m);

    neon_list* r = neon_list_concat(a, b); // consumes both

    PROVE(r->len == (size_t)n + m, "concat's length is the exact sum of the two");
    PROVE(r->len <= r->cap, "concat maintains len <= cap");
    // Identities were handed out 0..n-1 then n..n+m-1, so the concatenation must
    // read back as one run 0..n+m-1 -- order preserved across the seam.
    check_contents(r, 0, n + m);
    for (unsigned k = 0; k < MAXSLOTS; k++) {
        if (k >= n + m) break;
        PROVE(live[k] == 1, "concat leaves exactly one owned reference per element");
    }

    neon_release((neon_header*)r);
    check_all_released();
}

// 5. copy-on-write: pushing to a shared list must not disturb the other holder.
static void scenario_shared_cow(void) {
    unsigned n = NONDET_UPTO(3, "elements before sharing; the COW push makes 4, "
                                "so the copy fills its buffer exactly. Does not "
                                "cover a shared list that then grows again");

    neon_list* l = push_run(neon_list_new(&ELEM_W), 0, n);

    // A second holder. `push` now consumes one of the two references and must
    // copy rather than mutate in place.
    neon_retain((neon_header*)l);
    neon_list* keep = l;
    char* old_data = l->data;
    size_t old_len = l->len;

    neon_list* mut = push_owned(l, n);

    PROVE(mut != keep, "a push to a shared list copies instead of mutating");
    PROVE(keep->len == old_len, "the other holder's length is unchanged");
    PROVE(keep->data == old_data, "the other holder's buffer is neither moved nor freed");
    PROVE(keep->header.rc == 1, "the copy released the original exactly once");
    PROVE(mut->len == old_len + 1, "the copy carries the pushed element");
    PROVE(mut->len <= mut->cap, "the copy maintains len <= cap");

    check_contents(keep, 0, n);
    check_contents(mut, 0, n + 1);

    // Both lists own every shared element, so each identity below n is held twice.
    for (unsigned k = 0; k < MAXSLOTS; k++) {
        if (k >= n) break;
        PROVE(live[k] == 2, "the copy retained each shared element for itself");
    }
    PROVE(live[n] == 1, "the pushed element is owned exactly once");

    neon_release((neon_header*)mut);
    for (unsigned k = 0; k < MAXSLOTS; k++) {
        if (k >= n) break;
        PROVE(live[k] == 1, "dropping the copy leaves the original's references intact");
    }
    neon_release((neon_header*)keep);
    check_all_released();
}

// 6. eq / cmp: agreement, and the same slot arithmetic under a read-only walk.
static void scenario_eq_cmp(void) {
    unsigned n = NONDET_UPTO(3, "left length; comparison walks min(n, m) elements, "
                                "so 3 bounds the loop well under --unwind 12");
    unsigned m = NONDET_UPTO(3, "right length; same bound, and n != m reaches the "
                                "prefix case where length alone decides");

    // Both lists hold identities 0.., so one is always a prefix of the other and
    // the comparison is decided by length. That is the case where cmp's final
    // length tiebreak is load-bearing; unequal elements are elem_cmp's business.
    neon_list* a = push_run(neon_list_new(&ELEM_W), 0, n);
    neon_list* b = push_run(neon_list_new(&ELEM_W), 0, m);

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
static void scenario_capacity(void) {
    unsigned c = NONDET_UPTO(MAXN, "requested capacity, including 0 (which must "
                                   "leave data NULL) and capacities both above "
                                   "and below the number of pushes that follow");
    neon_list* l = neon_list_new_with_capacity(&ELEM_W, (int64_t)c);
    PROVE(l->len == 0, "a list built with capacity is still empty");
    PROVE(l->cap == c, "capacity is exactly what was requested");

    unsigned n = NONDET_UPTO(3, "pushes into a preallocated buffer; 3 straddles "
                                "the requested capacity in both directions, which "
                                "is what this scenario is for. The growth path "
                                "itself is scenario_push's job");
    for (unsigned i = 0; i < 3; i++) {
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

int main(void) {
    unsigned scenario = NONDET_UPTO(6,
        "harness dispatch only -- it selects which scenario runs and constrains "
        "no input the runtime sees. Every arm is verified in full");

    switch (scenario) {
        case 0: scenario_push(); break;
        case 1: scenario_at_oob(); break;
        case 2: scenario_set(); break;
        case 3: scenario_concat(); break;
        case 4: scenario_shared_cow(); break;
        case 5: scenario_eq_cmp(); break;
        default: scenario_capacity(); break;
    }
    return 0;
}
