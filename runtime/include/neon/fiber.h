#ifndef NEON_FIBER_H
#define NEON_FIBER_H

// A stackful, cooperatively-scheduled fiber — the execution half of the design in
// `docs/design/fibers.md`. Suspension is a stack swap (`neon_ctx_swap`, ~15 instructions of
// per-ABI assembly), so fibers are uncolored: a fiber that blocks looks like an ordinary
// call, no `async`.
//
// This is slice 2: the raw swap primitive and nothing above it. A fiber owns a stack and a
// suspended stack pointer; `neon_fiber_resume` runs one until it next yields or finishes,
// `neon_fiber_yield` hands control back to whoever resumed it. No scheduler, no run queue,
// no per-fiber arena yet — slices 3 and after. What this slice pins is that control transfer
// is correct, and correct *under AddressSanitizer*, whose fake-stack bookkeeping silently
// corrupts across a raw stack swap unless every switch is bracketed with the fiber
// annotations (`__sanitizer_start_switch_fiber` / `_finish_switch_fiber`).
//
// M:N-ready by construction: "who is running now" is a per-OS-thread `_Thread_local` (slice
// 3 pins it to a register), and a switch names both sides explicitly, so nothing assumes a
// single scheduler thread.

#include <stdbool.h>
#include <stddef.h>

typedef struct neon_fiber neon_fiber;

// The body a fiber runs. When it returns, the fiber is finished and control goes to the
// fiber that most recently resumed it (its resume-link). Results travel through channels
// (slice 5), not a return value.
typedef void (*neon_fiber_fn)(void* arg);

// Create a suspended fiber with its own stack (`stack_size` rounded up to a floor and
// aligned). It runs `fn(arg)` when first resumed. Nothing executes until then.
neon_fiber* neon_fiber_new(neon_fiber_fn fn, void* arg, size_t stack_size);

// The handle for the context running on this thread, the original thread context included.
// The first call on a thread adopts that thread's own stack as a fiber so control can be
// resumed back into it. Never NULL.
neon_fiber* neon_fiber_current(void);

// Resume `target` from the current fiber, recording the current fiber as `target`'s
// resume-link. Returns when `target` yields back or finishes. `target` must not be finished
// and must not be the current fiber.
void neon_fiber_resume(neon_fiber* target);

// Suspend the current fiber and return control to its resume-link (whoever last resumed it).
// Returns when the fiber is next resumed. Must not be called on the root thread fiber, which
// has no resume-link.
void neon_fiber_yield(void);

// Whether `f` has run its body to completion. A finished fiber must not be resumed.
bool neon_fiber_finished(const neon_fiber* f);

// Free a finished (or never-resumed) fiber and its stack. Freeing one that is suspended
// mid-body leaks whatever that body still owns — releasing those is the teardown walk's job
// (slice 4), not this primitive's.
void neon_fiber_free(neon_fiber* f);

#endif
