// Shared support for the CBMC models.
//
// Include this first in a model's `main.c`, before the runtime header.

#ifndef NEON_CBMC_SUPPORT_H
#define NEON_CBMC_SUPPORT_H

// CBMC provides these; declaring them keeps an editor's clang from reporting an
// implicit declaration in every model.
void __CPROVER_assume(int condition);
void __CPROVER_assert(int condition, const char* description);

// ---- the two things a model says ----

/// A property that must hold. The description is what CBMC prints, so write it as
/// the claim being made, not as a label: "rc is 1 + retains" reads usefully in a
/// failure report; "check rc" does not.
#define PROVE(cond, claim) __CPROVER_assert((cond), claim)

/// A restriction on the states explored — and therefore a hole in the proof.
///
/// The `why` is not passed to CBMC and exists to make you write it. That is the
/// point: an assumption silently narrows what was verified, so a model that
/// assumes away the case containing the bug still reports success and looks like
/// evidence. Every one of these is a sentence in the model's header comment
/// explaining what is consequently *not* covered.
#define ASSUME(cond, why) ((void)(why), __CPROVER_assume(cond))

/// An unconstrained `unsigned` bounded above, the idiom nearly every model needs
/// for "some number of operations, up to a few".
///
/// Bounded because CBMC unrolls loops: the bound must sit well under `--unwind`,
/// and `--unwinding-assertions` turns guessing too low into a failure rather than
/// a proof that quietly covers less than it claims.
#define NONDET_UPTO(max, why) ({                                                \
    unsigned _v = nondet_uint();                                                \
    ASSUME(_v <= (max), why);                                                    \
    _v;                                                                          \
})

// ---- making `_Noreturn` mean it ----

// `_exit` is libc, so CBMC has no body for it and assumes it *returns*. That
// matters more than it sounds: `neon_trap` ends in `_exit`, and every allocation
// in the runtime traps on failure. Without this stub CBMC walks off the end of a
// trap and carries on with the null pointer that caused it, reporting a pile of
// dereference failures inside `neon_alloc` that no real execution can reach.
//
// This is not asserting the trap is unreachable — a trap on out-of-memory is
// perfectly reachable and models should be free to reach one. It says only that
// nothing happens afterwards.
void _exit(int status) {
    (void)status;
    __CPROVER_assume(0);
}

void abort(void) {
    __CPROVER_assume(0);
}

// ---- nondeterministic values ----

// CBMC treats any function without a body as returning an unconstrained value, so
// these need no definition. Declaring them matters anyway: written undeclared,
// `nondet_uint()` is implicitly `int`, and `--conversion-check` then reports a
// signed-to-unsigned overflow in the *harness* — a failure that says nothing about
// the runtime and buries the ones that do.
unsigned nondet_uint(void);
int nondet_int(void);
unsigned long nondet_ulong(void);
_Bool nondet_bool(void);

#endif
