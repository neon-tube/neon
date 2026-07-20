#ifndef NEON_TRAP_H
#define NEON_TRAP_H

// Traps: print + _exit. No unwind, no teardown.

#include "neon/core.h"

// `_Noreturn` is C11 and is not a keyword in C++, so spelling it portably lets these headers
// be included from a C++ translation unit (the unit tests are C++). `[[noreturn]]` is the
// C++ spelling and sits in the same prefix position.
#ifdef __cplusplus
#define NEON_NORETURN [[noreturn]]
#else
#define NEON_NORETURN _Noreturn
#endif

NEON_NORETURN void neon_trap(const char* msg);
NEON_NORETURN void neon_panic(neon_str msg);
NEON_NORETURN void neon_unreachable(void);

#endif
