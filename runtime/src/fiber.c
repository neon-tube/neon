// `REG_RIP`/`REG_RSP` for the overflow handler's context rewrite are glibc GNU extensions;
// the define must precede the first libc include.
#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

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

#include "fiber_internal.h"
#include "internal.h" // neon_current_arena

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

// ---- guard-page stacks and stack-overflow isolation ----
//
// A fiber stack is mmap'd with a PROT_NONE guard page at its low (growth) end, so an overflow
// FAULTS on the guard instead of silently scribbling on the neighbouring heap. That much is
// unconditional (POSIX).
//
// Turning that fault into the same clean fiber-kill as any other trap is Linux/x86-64 and
// production-only (see the !NEON_ASAN gate): a SIGSEGV handler runs on a sigaltstack (the
// fiber's own stack is, by definition, exhausted), but it does NOT try to switch away from
// the signal handler — a non-returning handler leaves the altstack marked in-use and the
// signal blocked. Instead it REWRITES the interrupted context to resume, via the kernel's own
// sigreturn, on a healthy recovery stack (which unblocks the signal and clears the
// on-altstack state for free); that recovery routine then does the ordinary
// neon_fiber_on_trap switch. Under AddressSanitizer the handler is left OFF so ASan's own
// SEGV handler reports the overflow — it detects stack-overflow well, and replacing it would
// blind the whole test binary; the isolation path is validated by a standalone non-ASan
// program instead.

#if defined(__unix__) || (defined(__APPLE__) && defined(__MACH__))
#  define NEON_FIBER_GUARDED 1
#  include <sys/mman.h>
#  include <unistd.h>
#endif

#if defined(NEON_FIBER_GUARDED) && defined(__linux__) && defined(__x86_64__) && !NEON_ASAN
#  define NEON_FIBER_OVERFLOW_ISOLATION 1
#  include <signal.h>
#  include <ucontext.h>
#endif

#if defined(NEON_FIBER_GUARDED)
static size_t neon_fiber_page(void) {
    long p = sysconf(_SC_PAGESIZE);
    return p > 0 ? (size_t)p : 4096u;
}
#endif

// A fiber's stack floor. ASan-instrumented frames carry redzones and are deep, so a mean
// stack is generous on purpose; a caller asking for less gets this.
#define NEON_FIBER_MIN_STACK (64u * 1024u)

// struct neon_fiber lives in fiber_internal.h, shared with the scheduler.

// "Who is running" and "who we just left" are per-OS-thread — the design's current-fiber,
// reached through an initial-exec TLS load. `t_prev` lets a fresh fiber's bootstrap capture the
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
#else
    (void)exiting; // only ASan's fake-stack bookkeeping cares
#endif
    t_current = next;
    neon_current_arena = next->arena; // route neon_alloc/free to the fiber we're entering
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

void neon_fiber_on_trap(void) {
    // A trap fired somewhere in this fiber's call stack. We are still on that stack, but its
    // frames are being abandoned wholesale — C has no destructors to run, and the arena
    // objects those frames referenced are reclaimed by neon_fiber_free's bulk-drop. Mark the
    // fiber crashed-and-finished and switch back to the scheduler exactly as a normal exit
    // would, reusing the ASan-annotated swap (exiting=true discards this fiber's fake stack).
    // This is why crash isolation needs no setjmp/longjmp: killing a fiber IS a context
    // switch, and we already have a correct one.
    neon_fiber* self = t_current;
    self->crashed = true;
    self->finished = true;
    // A trap can fire INSIDE a transfer bracket (a mid-copy trap at a send, a task
    // return, a move-env copy). The bracket is thread-local state, like the current
    // arena — and unlike the arena, nothing downstream overwrites it, so a stale flag
    // would make the next fiber's first resource copy on this seat a phantom transfer.
    // Clearing it here is the crash-path pair of neon_transfer_end.
    neon_transfer_end();
    neon_fiber_switch(self, self->link, true);
    __builtin_unreachable();
}

#if defined(NEON_FIBER_OVERFLOW_ISOLATION)
#define NEON_FIBER_ALT_SIZE (64u * 1024u)
#define NEON_FIBER_RECOVERY_SIZE (64u * 1024u)

static _Thread_local bool t_sig_ready;
static _Thread_local void* t_altstack;  // sigaltstack the SEGV handler runs on
static _Thread_local void* t_recovery;  // healthy stack the handler redirects sigreturn onto
static volatile sig_atomic_t g_segv_installed; // process-wide; M=1 is single-threaded

// Runs on the recovery stack after sigreturn — a healthy stack, signal already unblocked.
// Reports the overflow with an async-signal-safe write and kills the fiber the usual way.
static void neon_fiber_overflow_recover(void) {
    static const char m[] = "neon: fiber stack overflow\n";
    ssize_t r = write(2, m, sizeof(m) - 1);
    (void)r;
    neon_fiber_on_trap(); // switch to the scheduler; never returns
    __builtin_unreachable();
}

static void neon_fiber_segv(int sig, siginfo_t* info, void* ucv) {
    neon_fiber* self = t_current;
    if (self != NULL && !self->is_root && t_recovery != NULL) {
        uintptr_t fault = (uintptr_t)info->si_addr;
        uintptr_t guard_hi = (uintptr_t)self->stack_bottom;   // usable low end
        uintptr_t guard_lo = guard_hi - neon_fiber_page();    // the PROT_NONE guard page
        if (fault >= guard_lo && fault < guard_hi) {
            // A fiber overflowed. Redirect the interrupted context to run recovery on the
            // healthy recovery stack; sigreturn does the rest. rsp ≡ 8 (mod 16) at entry.
            ucontext_t* uc = (ucontext_t*)ucv;
            uintptr_t rtop = ((uintptr_t)t_recovery + NEON_FIBER_RECOVERY_SIZE) & ~(uintptr_t)15u;
            uc->uc_mcontext.gregs[REG_RSP] = (greg_t)(rtop - 8u);
            uc->uc_mcontext.gregs[REG_RIP] = (greg_t)(uintptr_t)&neon_fiber_overflow_recover;
            return;
        }
    }
    // Not a fiber overflow: restore the default disposition and return, so the faulting
    // instruction re-executes into it (a genuine crash still cores, unchanged).
    struct sigaction dfl;
    memset(&dfl, 0, sizeof(dfl));
    dfl.sa_handler = SIG_DFL;
    sigemptyset(&dfl.sa_mask);
    sigaction(sig, &dfl, NULL);
}

// Idempotent per-thread setup for overflow isolation: a sigaltstack to catch the SEGV on (the
// fiber stack is exhausted), a healthy recovery stack, and — once per process — the handler.
static void neon_fiber_thread_init(void) {
    if (t_sig_ready) {
        return;
    }
    t_sig_ready = true;
    t_altstack = malloc(NEON_FIBER_ALT_SIZE);
    t_recovery = malloc(NEON_FIBER_RECOVERY_SIZE);
    if (t_altstack == NULL || t_recovery == NULL) {
        neon_trap("out of memory");
    }
    stack_t ss;
    ss.ss_sp = t_altstack;
    ss.ss_size = NEON_FIBER_ALT_SIZE;
    ss.ss_flags = 0;
    sigaltstack(&ss, NULL);
    if (!g_segv_installed) {
        g_segv_installed = 1;
        struct sigaction sa;
        memset(&sa, 0, sizeof(sa));
        sa.sa_sigaction = neon_fiber_segv;
        sa.sa_flags = SA_SIGINFO | SA_ONSTACK;
        sigemptyset(&sa.sa_mask);
        sigaction(SIGSEGV, &sa, NULL);
        sigaction(SIGBUS, &sa, NULL); // a guard-page hit can surface as SIGBUS on some kernels
    }
}
#else
static void neon_fiber_thread_init(void) {}
#endif

neon_fiber* neon_fiber_new(neon_fiber_fn fn, void* arg, size_t stack_size) {
    neon_fiber_thread_init(); // set up overflow isolation for this thread (idempotent, no-op under ASan)

    if (stack_size < NEON_FIBER_MIN_STACK) {
        stack_size = NEON_FIBER_MIN_STACK;
    }

    // Allocate the stack with a PROT_NONE guard page at the low (growth) end where it exists;
    // `base` is the whole allocation, `usable_lo`..`usable_lo+usable` the writable stack.
    void* base;
    char* usable_lo;
    size_t usable;
    size_t map_len = 0;
#if defined(NEON_FIBER_GUARDED)
    size_t page = neon_fiber_page();
    usable = (stack_size + page - 1) & ~(page - 1);
    map_len = usable + page; // one guard page below the usable region
    base = mmap(NULL, map_len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (base == MAP_FAILED) {
        neon_trap("out of memory");
    }
    if (mprotect(base, page, PROT_NONE) != 0) {
        neon_trap("mprotect failed");
    }
    usable_lo = (char*)base + page;
#else
    usable = (stack_size + 15u) & ~(size_t)15u;
    base = malloc(usable);
    if (base == NULL) {
        neon_trap("out of memory");
    }
    usable_lo = (char*)base;
#endif

    neon_fiber* f = calloc(1, sizeof(neon_fiber));
    if (f == NULL) {
        neon_trap("out of memory");
    }
    f->fn = fn;
    f->arg = arg;
    f->stack = base;
    f->map_len = map_len;
    f->stack_bottom = usable_lo;
    f->stack_size = usable;
    f->arena = neon_arena_create(); // the fiber's private heap; neon_alloc routes here while it runs

    // Prime the initial frame to mirror what neon_ctx_swap saves, so its first restore + ret
    // lands in the bootstrap. `top` is the 16-aligned stack top; `land` is rsp as the
    // bootstrap sees it (ABI: rsp ≡ 8 mod 16 at a function's entry); `sp` is where the gadget
    // begins restoring, 0x48 below `land` (16-byte FP slot + six saved registers + return
    // address). See the layout comment in fiber_swap_x86_64_sysv.S.
    uintptr_t top = ((uintptr_t)usable_lo + usable) & ~(uintptr_t)15u;
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

// Teardown pass 1: SEAL every live object with the immortal flag. From here on, any
// release aimed at it — a sibling's forced drop walking its fields, a resource cleanup
// releasing a capture — is a no-op through the guard `neon_release` has always had. That is
// what makes pass 2's forced drops exactly-once, and it costs the hot path NOTHING: the
// first cut instead taught `release` a teardown-mode test, and that one extra branch-with-
// TLS in the LTO-inlined release/drop recursion was measured at ~30% on binary-trees.
static void neon_fiber_teardown_seal(neon_header* h, void* ctx) {
    (void)ctx;
    h->flags |= NEON_IMMORTAL;
}

// Teardown pass 2: force the object dead. Its drop IS the per-type knowledge — it releases
// what the object holds (arena siblings are sealed, so only OUTGOING references really
// count down) and runs a resource's cleanup. rc = 0 first: an object being dropped is at
// zero — drop's contract everywhere, and a resource's finish asserts it. The remaining
// count was held entirely by this fiber (dead frames and arena siblings, by isolation), so
// forcing it is truthful, not a fudge.
static void neon_fiber_teardown_visit(neon_header* h, void* ctx) {
    (void)ctx;
    h->rc = 0;
    h->drop(h);
}

void neon_fiber_free(neon_fiber* f) {
    if (f == NULL || f->is_root) {
        return; // the root fiber lives with the thread and was never allocated here
    }
    // THE TEARDOWN WALK, then the bulk-free (docs/design/fibers.md). Every object still live
    // in the arena — leaked by a normal exit, or abandoned wholesale by a crash — gets its
    // drop called directly: a Resource runs its cleanup, a container releases what it holds.
    // Teardown mode (internal.h) is what makes those forced drops exactly-once and safe:
    // releasing an arena-internal reference is a no-op (it vaporizes in the bulk-free, and a
    // later object's drop cannot re-release an earlier, already-freed one), while a release
    // of a SHARED object — a channel handle, a received value's innards — really counts down.
    // Isolation is the soundness argument: no other fiber can reference this arena's objects,
    // so forcing every live one dead cannot strand an external holder.
    //
    // The walk runs on whatever context called free (the scheduler's, after a reap): the
    // current arena is cleared so a cleanup's allocations land in the shared slab, and the
    // teardown arena routes the drops' own frees back to the dying arena. Cleanups must not
    // block (a park on this context is a fatal trap) — the finalizer rule every runtime has.
    neon_arena* saved = neon_current_arena;
    neon_current_arena = NULL;   // a cleanup's allocations land in the shared slab
    neon_teardown_arena = f->arena; // the forced drops' own frees route home (lifecycle.c)
    neon_arena_walk(f->arena, neon_fiber_teardown_seal, NULL);
    neon_arena_walk(f->arena, neon_fiber_teardown_visit, NULL);
    // The abandoned body env (crash path only — normal exit cleared it): off-arena by
    // construction, so the walk above cannot find it. Released under teardown mode for the
    // same reason the walk runs under it — its drop may release refs into this arena
    // (no-ops now) alongside the shared/slab objects that really count down, including a
    // moved-in resource's owning ref, whose drop runs the cleanup right here.
    if (f->body_env != NULL) {
        neon_release(f->body_env);
        f->body_env = NULL;
    }
    neon_teardown_arena = NULL;
    neon_current_arena = saved;
    neon_arena_drop(f->arena);
#if defined(NEON_FIBER_GUARDED)
    munmap(f->stack, f->map_len);
#else
    free(f->stack);
#endif
    free(f);
}
