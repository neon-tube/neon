#include "libneon_rt.h"

#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

// ---- traps ----
//
// A trap prints to stderr and exits immediately with _exit: no atexit teardown, no
// unwind. The program is dying from a bug; the OS reclaims memory. Under NEON_DEBUG
// (a `-g` build) we abort() instead, so a debugger catches SIGABRT at the fault.

#define NEON_TRAP_CODE 101

_Noreturn void neon_trap(const char* msg) {
    // Flush stdout first: `_exit` skips stdio teardown, and output the program already
    // produced before the fault (its golden up to this point) must still be seen.
    fflush(stdout);
    fprintf(stderr, "neon: %s\n", msg);
    fflush(stderr);
#ifdef NEON_DEBUG
    abort();
#else
    _exit(NEON_TRAP_CODE);
#endif
}

_Noreturn void neon_panic(neon_str msg) {
    // Flush stdout first, for the same reason a trap does: `_exit` skips stdio teardown,
    // and whatever the program printed before failing must still be seen.
    fflush(stdout);
    fprintf(stderr, "neon: uncaught error: %.*s\n", (int)neon_str_len(&msg), neon_str_data(&msg));
    fflush(stderr);
    _exit(NEON_TRAP_CODE);
}

_Noreturn void neon_unreachable(void) {
    neon_trap("reached unreachable code");
}
