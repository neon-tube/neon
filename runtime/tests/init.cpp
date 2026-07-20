// Runtime setup that must happen before any test runs. minunit provides `main`, and a
// global constructor runs before it, so `neon_rt_init` is called once ahead of every suite.
// It is a no-op today, but calling it keeps the tests honest about the runtime's contract:
// initialise, then use.

extern "C" {
#include "libneon_rt.h"
}

namespace {
struct RtInit {
    RtInit() { neon_rt_init(); }
} rt_init;
} // namespace
