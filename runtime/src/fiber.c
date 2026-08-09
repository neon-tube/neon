// Stackful cooperative fibers over the `neon_ctx_swap` gadget. This file is the C half: it
// primes a fresh fiber's stack so the gadget's first `ret` lands in a bootstrap, tracks
// which fiber is running on this thread, and — the part that is easy to get subtly wrong —
// brackets every stack swap with AddressSanitizer's fiber annotations.
//
// Why the annotations are not optional under ASan: ASan gives each stack a "fake stack" for
// use-after-return detection and assumes stacks do not move under it. A raw `neon_ctx_swap`
// moves the stack out from under ASan, so without telling it, ASan keeps attributing the new
// fiber's frames to the old fiber's fake stack and reports phantom use-after-return / stack
// corruption. `__sanitizer_start_switch_fiber` before the swap and `_finish_switch_fiber`
// after hand ASan the incoming stack's bounds and stash/restore the outgoing fiber's fake
// stack. When not built under ASan these compile to nothing.

#include "libneon_rt.h"

#include "neon/fiber.h"

#include <stdint.h>
#include <stdlib.h>
#include <string.h>

// The assembly gadget: save this context, store its sp through `from_sp`, load `to_sp`.
extern void neon_ctx_swap(void** from_sp, void* to_sp);

// ---- AddressSanitizer fiber annotations ----

#if defined(__has_feature)
#  if __has_feature(address_sanitizer)
#    define NEON_ASAN 1
#  endif
#endif
#if defined(__SANITIZE_ADDRESS__)
#  define NEON_ASAN 1
#endif
#ifndef NEON_ASAN
#  define NEON_ASAN 0
#endif

#if NEON_ASAN
// Declared here rather than via <sanitizer/common_interface_defs.h> so this file needs no
// sanitizer headers on the include path; the ABI is stable.
void __sanitizer_start_switch_fiber(void** fake_stack_save, const void* bottom, size_t size);
void __sanitizer_finish_switch_fiber(void* fake_stack_save, const void** bottom_old,
                                     size_t* size_old);
#endif

// A fiber's stack floor. ASan-instrumented frames carry redzones and are deep, so a mean
// stack is generous on purpose; a caller asking for less gets this.
#define NEON_FIBER_MIN_STACK (64u * 1024u)

struct neon_fiber {
    void* sp;                 // suspended stack pointer — valid exactly while not running
    void* stack;              // the heap stack allocation; NULL for the adopted root fiber
    const void* stack_bottom; // low address of the usable stack, for the ASan annotations
    size_t stack_size;
    void* fake_stack;         // ASan fake-stack save slot while this fiber is switched-away
    neon_fiber_fn fn;
    void* arg;
    neon_fiber* link;         // resume-link: control returns here on yield or finish
    bool finished;
    bool is_root;             // the thread's original context, adopted rather than allocated
};

// "Who is running" and "who we just left" are per-OS-thread — the design's current-fiber,
// which slice 3 pins to a register. `t_prev` lets a fresh fiber's bootstrap capture the
// resumer's stack region for ASan (see below).
static _Thread_local neon_fiber* t_current;
static _Thread_local neon_fiber* t_prev;
static _Thread_local neon_fiber t_root;
static _Thread_local bool t_root_init;

neon_fiber* neon_fiber_current(void) {
    if (!t_root_init) {
        t_root_init = true;
        t_root.is_root = true;
        // The root's own stack bounds are unknown here and are filled in the first time a
        // fiber is started from this thread (that fiber's bootstrap captures them via ASan's
        // finish out-params). Nothing switches *to* the root before that happens.
        t_current = &t_root;
    }
    return t_current;
}

// The one place a stack swap happens. `exiting` means `cur` will never run again, so its
// fake stack is discarded (NULL save) rather than stashed.
static void neon_fiber_switch(neon_fiber* cur, neon_fiber* next, bool exiting) {
    t_prev = cur;
#if NEON_ASAN
    __sanitizer_start_switch_fiber(exiting ? NULL : &cur->fake_stack, next->stack_bottom,
                                   next->stack_size);
#endif
    t_current = next;
    neon_ctx_swap(&cur->sp, next->sp);
    // Resumed as `cur`. (Never reached when `exiting`.)
#if NEON_ASAN
    __sanitizer_finish_switch_fiber(cur->fake_stack, NULL, NULL);
#endif
}

// Entered by the gadget's `ret` on a fresh fiber's first run. Reached as-if-called (the
// primed frame lands rsp at a 16-byte-minus-8 boundary, the ABI's function-entry state), so
// this is an ordinary C function with no arguments — it recovers itself from `t_current`.
static void neon_fiber_bootstrap(void) {
    neon_fiber* self = t_current;
#if NEON_ASAN
    // First arrival on this stack: finish the switch with no prior fake stack of our own, and
    // capture the resumer's region (t_prev) so a later switch back to it is well-formed. This
    // is where the root fiber's unknown bounds get recorded.
    __sanitizer_finish_switch_fiber(NULL, &t_prev->stack_bottom, &t_prev->stack_size);
#endif
    self->fn(self->arg);
    self->finished = true;
    // Hand control to the resume-link and never come back; the link must not resume a
    // finished fiber, so the return below is unreachable.
    neon_fiber_switch(self, self->link, true);
    __builtin_unreachable();
}

neon_fiber* neon_fiber_new(neon_fiber_fn fn, void* arg, size_t stack_size) {
    if (stack_size < NEON_FIBER_MIN_STACK) {
        stack_size = NEON_FIBER_MIN_STACK;
    }
    stack_size = (stack_size + 15u) & ~(size_t)15u;

    void* stack = malloc(stack_size); // malloc is 16-byte aligned on x86-64 SysV
    if (stack == NULL) {
        neon_trap("out of memory");
    }
    neon_fiber* f = calloc(1, sizeof(neon_fiber));
    if (f == NULL) {
        neon_trap("out of memory");
    }
    f->fn = fn;
    f->arg = arg;
    f->stack = stack;
    f->stack_bottom = stack;
    f->stack_size = stack_size;

    // Prime the initial frame to mirror what neon_ctx_swap saves, so its first restore + ret
    // lands in the bootstrap. `top` is the 16-aligned stack top; `land` is rsp as the
    // bootstrap sees it (ABI: rsp ≡ 8 mod 16 at a function's entry); `sp` is where the gadget
    // begins restoring, 0x48 below `land` (16-byte FP slot + six saved registers + return
    // address). See the layout comment in fiber_swap_x86_64_sysv.S.
    uintptr_t top = ((uintptr_t)stack + stack_size) & ~(uintptr_t)15u;
    uintptr_t land = top - 8u;
    uintptr_t sp = land - 0x48u;
    unsigned char* frame = (unsigned char*)sp;
    memset(frame, 0, (size_t)(top - sp)); // zero saved regs, FP slot, and the guard return
    *(uint32_t*)(frame + 0x00) = 0x1f80u; // MXCSR default: every FP exception masked
    *(uint16_t*)(frame + 0x08) = 0x037fu; // x87 control-word default
    *(void**)(frame + 0x40) = (void*)&neon_fiber_bootstrap; // gadget's `ret` target
    f->sp = (void*)sp;
    return f;
}

void neon_fiber_resume(neon_fiber* target) {
    neon_fiber* cur = neon_fiber_current();
    if (target == cur) {
        neon_trap("neon_fiber_resume: cannot resume the current fiber");
    }
    if (target->finished) {
        neon_trap("neon_fiber_resume: fiber has finished");
    }
    target->link = cur;
    neon_fiber_switch(cur, target, false);
}

void neon_fiber_yield(void) {
    neon_fiber* cur = neon_fiber_current();
    if (cur->is_root) {
        neon_trap("neon_fiber_yield: the root fiber has nothing to yield to");
    }
    neon_fiber_switch(cur, cur->link, false);
}

bool neon_fiber_finished(const neon_fiber* f) {
    return f->finished;
}

void neon_fiber_free(neon_fiber* f) {
    if (f == NULL || f->is_root) {
        return; // the root fiber lives with the thread and was never allocated here
    }
    free(f->stack);
    free(f);
}
