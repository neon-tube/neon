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

NEON_NORETURN void neon_trap_oob(int64_t index, size_t len) {
    // The out-of-bounds trap, carrying the two numbers every report needs. Its own
    // entry point rather than callers formatting a message: a trap must not allocate,
    // and `fprintf` formats into stderr's own buffer. The stdlib's CATCHABLE
    // IndexError spells the same facts ("index 9 out of range for length 2"); a trap
    // saying less than the throw did was a debugging tax with no payer.
    fflush(stdout);
    fprintf(stderr, "neon: list index %lld out of range for length %zu\n",
            (long long)index, len);
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
