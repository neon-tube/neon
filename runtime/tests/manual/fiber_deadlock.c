// Deadlock detection (slice 5). A fiber receives on a channel nobody sends to: it parks, the
// run queue empties with it still live, and the scheduler must report a deadlock and trap
// rather than hang silently. Not a tinyunit test — the trap terminates the process (exit
// 101), which a forked test child would read as a failure. Build it from runtime/ the same
// way as tests/manual/fiber_overflow.c (add src/fiber_chan.c to the source list) and run:
//
//   ./a.out ; echo "exit=$?"   # expect: "neon: deadlock: all fibers are blocked", exit 101
#include "libneon_rt.h"
#include <stdio.h>
static neon_chan* ch;
static void waiter(void* a) {
    (void)a;
    void* v;
    neon_chan_recv(ch, &v); // blocks forever — nothing will ever send
}
static void boss(void* a) {
    (void)a;
    ch = neon_chan_new();
    neon_fiber_spawn(waiter, NULL);
}
int main(void) {
    neon_rt_init(0, NULL);
    neon_fiber_runtime(boss, NULL); // must not return: the scheduler traps on deadlock
    printf("MUST NOT REACH\n");
    return 0;
}
