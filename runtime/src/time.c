// The clocks behind `std::time`, each a one-line door to the platform seam. Two clocks
// with names that say which, because the classic bug is measuring with the wall clock:
// `monotonic` never goes backwards and its zero means nothing; `unix_millis` is the
// calendar and can jump any direction when the machine's clock is set.

#include "libneon_rt.h"

#include "platform.h"

int64_t neon_time_monotonic(void) {
    return neon_plat_monotonic_ns();
}

int64_t neon_time_unix_millis(void) {
    return neon_plat_unix_millis();
}

_Thread_local void (*neon_fiber_sleep_hook)(int64_t millis) = NULL;

void neon_time_sleep(int64_t millis) {
    // Inside a fiber runtime, sleeping means parking THIS FIBER on a deadline while the
    // scheduler runs everyone else; the hook (internal.h) is armed exactly while a runtime
    // is active. Everywhere else this is the plain blocking sleep it always was.
    if (neon_fiber_sleep_hook != NULL) {
        neon_fiber_sleep_hook(millis);
        return;
    }
    neon_plat_sleep_ms(millis);
}
