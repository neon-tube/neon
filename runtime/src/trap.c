#include "libneon_rt.h"

#include "platform.h"

#include <stdio.h>
#include <stdlib.h>

// ---- traps ----
//
// A trap prints to stderr and exits immediately with `neon_plat_exit_now`: no atexit
// teardown, no unwind. The program is dying from a bug; the OS reclaims memory. Under
// NEON_DEBUG (a `-g` build) we abort() instead, so a debugger catches SIGABRT at the fault.

#define NEON_TRAP_CODE 101

NEON_NORETURN void neon_trap(const char* msg) {
    // Flush stdout first: exiting this way skips stdio teardown, and output the program
    // already produced before the fault (its golden up to this point) must still be seen.
    fflush(stdout);
    fprintf(stderr, "neon: %s\n", msg);
    fflush(stderr);
#ifdef NEON_DEBUG
    abort();
#else
    neon_plat_exit_now(NEON_TRAP_CODE);
#endif
}

NEON_NORETURN void neon_panic(neon_str msg) {
    // Flush stdout first, for the same reason a trap does: exiting this way skips stdio
    // teardown, and whatever the program printed before failing must still be seen.
    fflush(stdout);
    fprintf(stderr, "neon: uncaught error: %.*s\n", (int)neon_str_len(&msg), neon_str_data(&msg));
    fflush(stderr);
    neon_plat_exit_now(NEON_TRAP_CODE);
}

NEON_NORETURN void neon_unreachable(void) {
    neon_trap("reached unreachable code");
}
