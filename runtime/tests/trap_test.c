// `runtime/src/trap.c`: the three ways the runtime kills a program from inside a bug --
// `neon_trap` (a runtime fault with a message), `neon_unreachable` (a branch codegen proved
// dead), and `neon_panic` (an uncaught Neon error). All exit with the trap status and never
// return. `EXPECT_TRAP` runs the statement in a forked child and asserts that child exits
// with exactly that status, so these are observable without taking the harness down.

#include "tinyunit.h"

#include "support.h"

TEST_SUITE("trap");

TEST(trap_exits_with_the_trap_status) {
    EXPECT_TRAP(neon_trap("something went wrong"));
}

TEST(unreachable_traps) {
    // A `_Noreturn` with no message of its own -- it routes through `neon_trap`.
    EXPECT_TRAP(neon_unreachable());
}

TEST(panic_traps_on_an_uncaught_error) {
    // Takes a `neon_str`; an owned one is fine, since the child exits before any release
    // would be owed (and `_exit` skips the leak check).
    EXPECT_TRAP(neon_panic(nt_owned("uncaught")));
}
