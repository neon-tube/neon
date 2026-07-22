// Model: `neon_str_add` and `neon_str_concat` on two heap strings, watching the operands'
// reference counts across each call.
//
// THE INVARIANT: both build the same byte-exact concatenation as a freshly owned string
// (rc == 1), but they differ in ownership and must differ in exactly this way -- `add`
// BORROWS its operands (their counts are untouched) while `concat` CONSUMES them (each is
// released once). Nothing else about the two functions differs; they are otherwise identical
// code.
//
// This is the string counterpart to the list model's borrow/consume split, and the reason
// it is worth a machine check is that the two conventions sit one line apart and a wrong one
// is invisible at the call site. `add` is the `+` operator, whose operands the refcount pass
// releases itself at their last use, so an `add` that also released them would double-free
// every `a + b` in the language. `concat` is the native, which owns its arguments and must
// release them, so a `concat` that forgot would leak every one. Each bug is a count that is
// wrong by one in opposite directions, and only becomes a fault later -- at the pass's own
// release, or never -- which is precisely what a refcount assertion at the call catches and
// a runtime test usually does not.
//
// The operands are HEAP strings with real owners (rule 7): a literal has `owner == NULL` and
// `neon_str_release` on it is a no-op, so a stray release inside either function would be
// invisible. Here each operand carries a `neon_header` with a live count, and the harness
// pins an extra reference to each so the balance is read as a count -- `rc == 0` is one
// release too many (a double-free of the operand), `rc == 2` is one too few (a leak) --
// caught at the imbalance rather than at a later use. Lengths are concrete so the result
// `memcpy` has a constant count (CBMC's `memcpy` is imprecise with a symbolic one, README
// rule 4); the bytes themselves are unconstrained, so the concatenation is proved byte-exact
// over arbitrary content.
//
// Verifies `src/string.c` compiled from source; see rule 1.
//
// ---- VALIDATED BY MUTATION (rule 6) ----
//
// Baseline: 481 properties, VERIFICATION SUCCESSFUL. Five mutations, each reverted after.
//
// 1. `neon_str_add` releasing its operands -- a borrow turned into a consume, the double-free
//    every `a + b` would become. Failed 4 of 481 on "add borrows: neither operand's count
//    changed", and then as the shape of the fault: `h->rc - 1` underflows and the harness's
//    own later release is a deallocated-object dereference. This is the bug class the counted
//    operands and pins exist to make loud.
//
// 2. `neon_str_concat` releasing neither operand -- a consume turned into a leak. Failed 2 of
//    481 on "concat consumes: each operand released once" and the memory-leak check.
//
// 3. `neon_str_concat` releasing only the right operand. Failed the same 2 -- one operand is
//    still leaked and its count is one too high.
//
// 4. The second copy landing at `data + lb` instead of `data + la`. Failed 4 of 477 on "and
//    continues with the right operand's bytes" and, because the wrong offset runs the copy
//    off the result buffer, on a `memcpy` destination-writeable bounds check.
//
// 5. The result length recorded as `la` instead of `la + lb`. Failed 2 of 479 on "the result
//    is the concatenation's length".
//
// ---- SCOPE: what this model does not cover ----
//
// 1. FIXED SMALL LENGTHS. The operands are 2 and 3 bytes, so the result is 5 -- enough to
//    exercise both source offsets of the copy and stay on `neon_str_new`'s short path -- but
//    the length arithmetic (`la + lb`, the offset `data + la`) is checked at one pair of
//    lengths, not symbolically. A symbolic length would put a symbolic count on the
//    `memcpy` and fail spuriously (SCOPE reason, README rule 4). The empty-operand case is
//    not driven here.
//
// 2. NO GROWTH OR REALLOC. Each operand is a single fresh allocation; neither function
//    resizes, so nothing here addresses a buffer that has moved.
//
// 3. OUT-OF-MEMORY IS A CUT PATH, not a return: `neon_alloc` traps on a NULL `malloc`, so
//    the concatenation has no failure return. `--malloc-may-fail --malloc-fail-null` buys
//    the check that the trap terminates rather than running on with a NULL buffer.
//
// 4. THE OTHER STRING CONSTRUCTORS (`repeat`, `join`, `slice`, ...) share this consume
//    convention but not this code, and are not covered here; `join`'s list consumption in
//    particular is its own contract.
//
// 5. SINGLE-THREADED: the counts are plain `uint64_t`.

#include "../support/cbmc_support.h"
#include "libneon_rt.h"

// `neon_str_new` -- the runtime's own heap-string constructor -- is a `static inline` in the
// internal header, not the public ABI, because generated code builds strings through the
// operations rather than raw. It is still shipping runtime source (rule 1), and a model is
// just one more translation unit that may use it, exactly as `string.c` does.
#include "../../src/internal.h"

#include <stdio.h>

// Rule 4. `neon_trap` (reached from every allocation's OOM branch under `--malloc-fail-null`)
// calls `fflush`/`fprintf`, and CBMC's models of those pull a `FILE` into each trap site.
int fprintf(FILE* stream, const char* fmt, ...) { (void)stream; (void)fmt; return 0; }
int fflush(FILE* stream) { (void)stream; return 0; }

#define LA 2
#define LB 3

// A fresh heap string of concrete length `n` over unconstrained bytes. `neon_str_new`
// allocates it with `rc == 1`; the caller then pins a second reference so the operand's
// count is observable across the call under test.
static char abuf[LA];
static char bbuf[LB];

// A nondet `char` directly, rather than `(char)nondet_uint()`: the cast would be an
// unsigned-to-signed narrowing that `--conversion-check` flags for any byte over 127, and
// the byte values are irrelevant to this model's ownership and copy claims anyway.
char nondet_char(void);

int main(void) {
    for (size_t i = 0; i < LA; i++) abuf[i] = nondet_char();
    for (size_t i = 0; i < LB; i++) bbuf[i] = nondet_char();

    // ---- add: borrows both ----
    {
        neon_str a = neon_str_new(abuf, LA);
        neon_str b = neon_str_new(bbuf, LB);
        PROVE(a.owner->rc == 1 && b.owner->rc == 1, "each operand starts uniquely owned");
        neon_retain(a.owner); // harness pin -> rc 2
        neon_retain(b.owner);

        neon_str r = neon_str_add(a, b);

        PROVE(a.owner->rc == 2 && b.owner->rc == 2,
              "add borrows: neither operand's count changed");
        PROVE(r.owner != NULL && r.owner->rc == 1, "the result is a fresh, uniquely owned string");
        PROVE(neon_str_len(&r) == LA + LB, "the result is the concatenation's length");
        const char* rd = neon_str_data(&r);
        for (size_t i = 0; i < LA; i++)
            PROVE(rd[i] == abuf[i], "the result opens with the left operand's bytes");
        for (size_t i = 0; i < LB; i++)
            PROVE(rd[LA + i] == bbuf[i], "and continues with the right operand's bytes");

        // The harness still owns its pins and the fresh result; release all three so the
        // leak check sees a clean slate before the concat half runs.
        neon_release(r.owner);
        neon_release(a.owner); // pin
        neon_release(b.owner); // pin
        neon_release(a.owner); // the reference add borrowed and did not take
        neon_release(b.owner);
    }

    // ---- concat: consumes both ----
    {
        neon_str a = neon_str_new(abuf, LA);
        neon_str b = neon_str_new(bbuf, LB);
        neon_retain(a.owner); // harness pin -> rc 2, so a consume lands it back at 1
        neon_retain(b.owner);

        neon_str r = neon_str_concat(a, b);

        PROVE(a.owner->rc == 1 && b.owner->rc == 1,
              "concat consumes: each operand released once, leaving only the harness pin");
        PROVE(r.owner != NULL && r.owner->rc == 1, "the result is a fresh, uniquely owned string");
        PROVE(neon_str_len(&r) == LA + LB, "the result is the concatenation's length");
        const char* rd = neon_str_data(&r);
        for (size_t i = 0; i < LA; i++)
            PROVE(rd[i] == abuf[i], "the result opens with the left operand's bytes");
        for (size_t i = 0; i < LB; i++)
            PROVE(rd[LA + i] == bbuf[i], "and continues with the right operand's bytes");

        neon_release(r.owner);
        neon_release(a.owner); // pin -> operand dropped exactly here
        neon_release(b.owner);
    }
    // Nothing leaked by hand: --memory-leak-check asserts every allocation above was freed.
    return 0;
}
