// End-to-end validation of fiber stack-overflow ISOLATION (slice 4c). Not part of the
// tinyunit suite: that runs under ASan, where the overflow handler is deliberately off (ASan
// catches the overflow itself), and each test is a forked child whose signal-death would read
// as a failure. Built WITHOUT ASan, the guard page + SIGSEGV handler turn an overflow into a
// clean fiber-kill, exactly like any other trap. This program overflows one fiber and checks
// the process survives and a sibling still runs — exit 0 means isolation worked, a
// signal-death means it did not.
//
// Run it from runtime/ (compiles the runtime from source, no ASan so the handler is live):
//
//   cc -O2 -Iinclude -o /tmp/fiber_overflow tests/manual/fiber_overflow.c \
//     src/lifecycle.c src/arena.c src/trap.c src/arith.c src/string.c src/resource.c \
//     src/file.c src/math.c src/io.c src/list.c src/any.c src/encoding.c src/map.c \
//     src/os.c src/process.c src/random.c src/time.c src/fiber.c src/fiber_sched.c \
//     src/fiber_swap_x86_64_sysv.S -lm
//   /tmp/fiber_overflow ; echo "exit=$?"
//
// Expected: the "fiber stack overflow" line, then the survivor, then PASS, exit 0.

#include "libneon_rt.h"

#include <stdio.h>
#include <stdlib.h>

static volatile int sink;

// Unbounded recursion with a real frame each level, written so the compiler cannot
// tail-call-optimise it into a loop: the frame's `pad` is used AFTER the recursive call and
// the call's result is returned, so each level's frame must persist and the stack really
// grows into the guard page.
__attribute__((noinline)) static int blow_the_stack(int n) {
    volatile char pad[512];
    pad[0] = (char)n;
    pad[511] = (char)n;
    int deeper = blow_the_stack(n + 1);
    return deeper + pad[0] + pad[511]; // uses pad past the call → no TCO
}

static void overflowing_fiber(void* arg) {
    (void)arg;
    printf("  overflowing fiber: about to recurse forever\n");
    fflush(stdout);
    sink += blow_the_stack(0);
    printf("  overflowing fiber: THIS MUST NOT PRINT\n");
}

static int survivor_ran = 0;
static void survivor(void* arg) {
    (void)arg;
    survivor_ran = 1;
    printf("  survivor fiber: ran after the overflow — isolation worked\n");
}

static void boss(void* arg) {
    (void)arg;
    neon_fiber_spawn(overflowing_fiber, NULL); // crashes on overflow
    neon_fiber_spawn(survivor, NULL);          // must still run
}

int main(void) {
    neon_rt_init(0, NULL);
    neon_fiber_runtime(boss, NULL);
    if (survivor_ran) {
        printf("main: PASS — overflow isolated, program survived\n");
        return 0;
    }
    printf("main: FAIL — survivor did not run\n");
    return 1;
}
