// Runtime setup that must happen before any test runs. tinyunit provides `main`, and a
// global constructor runs before it, so `neon_rt_init` is called once ahead of every suite.
// It is a no-op today, but calling it keeps the tests honest about the runtime's contract:
// initialise, then use.

#include "libneon_rt.h"

__attribute__((constructor)) static void neon_rt_test_init(void) { neon_rt_init(); }
