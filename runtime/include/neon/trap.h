#ifndef NEON_TRAP_H
#define NEON_TRAP_H

// Traps: print + _exit. No unwind, no teardown.

#include "neon/core.h"
#include "neon/portability.h" // NEON_NORETURN

NEON_NORETURN void neon_trap(const char* msg);
// The out-of-bounds trap: same exit, message carries the index and the length.
NEON_NORETURN void neon_trap_oob(int64_t index, size_t len);
NEON_NORETURN void neon_panic(neon_str msg);
NEON_NORETURN void neon_unreachable(void);

#endif
