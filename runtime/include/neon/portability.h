#ifndef NEON_PORTABILITY_H
#define NEON_PORTABILITY_H

// The compiler-dialect spellings, in one place.
//
// Everything here is a *spelling* difference, not a behavioural one: the same C in three
// vocabularies. GCC and Clang share one, MSVC has its own, and C++ has a third (these
// headers are includable from a C++ translation unit). A construct that only one compiler
// can express does not belong here — it belongs behind a real abstraction.
//
// This is a public header, not `src/platform.h`: generated C includes `libneon_rt.h` and
// emits `NEON_INLINE` on every function the IR marked for inlining, so the spelling has to
// travel with the ABI rather than stay inside the runtime's own sources.
//
// Clang defines `_MSC_VER` when it is impersonating MSVC (clang-cl), so every test below
// asks for MSVC *and not clang* — clang-cl still understands the GNU attribute syntax, and
// preferring it keeps clang on one path whichever driver invoked it.
#if defined(_MSC_VER) && !defined(__clang__)
#define NEON_MSVC 1
#endif

// A function that never returns. `_Noreturn` is C11 and is not a keyword in C++; MSVC's C
// front end only accepts it under `/std:c11` and up, so it gets `__declspec` instead.
// All three sit in the same prefix position, which is what lets one macro cover them.
#ifdef __cplusplus
#define NEON_NORETURN [[noreturn]]
#elif defined(NEON_MSVC)
#define NEON_NORETURN __declspec(noreturn)
#else
#define NEON_NORETURN _Noreturn
#endif

// The whole qualifier for a function the backend decided to inline unconditionally, not
// just the attribute: MSVC's `__forceinline` *is* `inline`, and spelling both warns
// (C4141). Covering `static` too keeps the one difference in one macro.
//
// `static` as well as the attribute is deliberate and not merely tidy. `always_inline` on
// an externally visible function still forces an out-of-line copy to exist, and gcc warns
// when it cannot reconcile the two; internal linkage is correct here anyway, since a whole
// Neon program is emitted as one translation unit.
#ifdef NEON_MSVC
#define NEON_INLINE static __forceinline
#else
#define NEON_INLINE static inline __attribute__((always_inline))
#endif

#endif
