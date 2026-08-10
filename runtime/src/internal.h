#ifndef NEON_INTERNAL_H
#define NEON_INTERNAL_H

// Runtime internals shared between the runtime's own `.c` files. This is *not* part of the
// ABI: generated C includes `libneon_rt.h` and never this file, which is why it
// lives in `src/` and is not installed.
//
// The helpers here are `static inline` rather than plain externs on purpose. They were
// `static` in the single-file runtime, so giving them external linkage would add symbols to
// the archive that the ABI does not promise; keeping them inline preserves the exported
// symbol set exactly while letting more than one translation unit use them.

#include "neon/arena.h"
#include "neon/core.h"
#include "neon/lifecycle.h"

#include "platform.h" // neon_ssize/neon_iovec, for the blocking-IO hook signatures

// The current fiber's arena, or NULL on the root / non-fiber context. `neon_alloc` and
// `neon_free` route through it (see src/lifecycle.c and docs/design/fibers.md); the fiber
// scheduler sets it on every context switch. Defined in lifecycle.c — which is always
// compiled — so a build without the fiber sources (non-x86-64, or Windows) still links with
// it permanently NULL, and only the fiber code ever stores a non-NULL value.
//
// initial-exec TLS model, not the default general-dynamic: this pointer is read on the
// hottest path there is (once per neon_alloc), and general-dynamic pays a __tls_get_addr
// call every time — ~7% on binary-trees, measured. initial-exec is a single %fs-relative
// load and is sound because the runtime is always statically linked into the program
// executable, never dlopen'd, so its TLS block exists at process start. (The design's
// register-pinned arena would drop even this load; initial-exec is the cheap 90%.)
// THE TLS RULE, since this is the thing most likely to be misremembered as "no TLS": the
// runtime uses thread-local state freely — an M:N runtime is per-thread state — and what
// was measured and rejected is narrower than that. Two findings, both on binary-trees:
//
//   * general-dynamic TLS (the default model) on the ALLOCATION path costs 6-7%, because
//     each access is a __tls_get_addr CALL. initial-exec is one %fs-relative load and
//     costs ~1%. Hence NEON_TLS_IE on the three hot ones — the current arena, the teardown
//     arena, the slab free lists — and only those: the annotation forces a fixed TLS
//     offset, which is not free at link time and buys nothing for a variable read beside a
//     syscall.
//   * ANY addition to neon_retain/neon_release costs 30-44%, TLS or not. That pair is
//     measured surface and carries nothing new; see the comment above it.
//
// Everything else — the scheduler seat, the current fiber, the preemption flag, the seat's
// io_uring, the sleep/trap/IO hooks — is ordinary _Thread_local at the default model, read
// beside a context switch or a syscall where the cost is unmeasurable.
#if defined(__GNUC__) && !defined(NEON_CBMC)
#  define NEON_TLS_IE __attribute__((tls_model("initial-exec")))
#else
#  define NEON_TLS_IE
#endif
extern _Thread_local NEON_TLS_IE neon_arena* neon_current_arena;

// The send-routing sentinel: `neon_send_routing_begin` points the current arena HERE, and
// `neon_alloc` routes such allocations to the slab WITH the SHARED flag (atomic rc). A real
// pointer is never 1, and the non-fiber fast path (`arena == NULL`) is untouched — the
// sentinel test lives inside the already-taken fiber branch.
#define NEON_SHARED_ARENA ((neon_arena*)1)

// Teardown mode: non-NULL exactly while a dying fiber's arena is being WALKED (the
// docs/design/fibers.md teardown). Two effects, both in lifecycle.c: releasing an
// ARENA-flagged object is a NO-OP (internal references vaporize in the coming bulk-free,
// and no-oping them is what makes the walk's per-object drops exactly-once — a later
// object's drop cannot re-release an earlier, already-freed one), and freeing an
// ARENA-flagged object routes to THIS arena (the walk runs on the scheduler's context,
// where neon_current_arena is NULL). Only the fiber teardown sets it.
extern _Thread_local NEON_TLS_IE neon_arena* neon_teardown_arena;

// The fiber-sleep bridge, the trap-handler pattern again: while a fiber runtime is active
// the scheduler arms this per-thread hook, and `time::sleep` routes through it — parking
// just the calling FIBER on a deadline instead of blocking the whole thread. NULL in every
// non-fiber program (sleep is the plain nanosleep it always was), and the hook itself falls
// back to the plain sleep when called on the root context (a resource cleanup, say).
extern _Thread_local void (*neon_fiber_sleep_hook)(int64_t millis);

// The blocking-file-IO bridge (same pattern): while a fiber runtime is active these route a
// regular-file read / writev batch through the offload pool — the calling FIBER parks, a
// worker thread runs the raw syscall — and NULL means the plain blocking call, everywhere
// else. `neon_ssize`/`neon_iovec` come from src/platform.h, included just above.
extern _Thread_local neon_ssize (*neon_fiber_blocking_read)(int fd, void* buf, size_t n);
extern _Thread_local neon_ssize (*neon_fiber_blocking_writev)(int fd, const neon_iovec* iov,
                                                              int n);

// Park the calling fiber until `pid` exits, without blocking the thread — pidfd + epoll on
// Linux; NULL (a plain blocking waitpid) everywhere else and off-runtime.
extern _Thread_local void (*neon_fiber_pidwait_hook)(int64_t pid);

// Wait for a descriptor to become readable (`for_write` false) or writable, parking the
// CALLING FIBER rather than the thread. What `std::net` blocks on: every socket is
// non-blocking underneath, so an operation that would block instead waits here and retries.
// Returns 0 on readiness, or a negative errno. NULL off-runtime, where net.c falls back to
// an ordinary poll(2) — which is why sockets work identically outside a fiber.
extern _Thread_local int (*neon_fiber_readiness_hook)(int fd, bool for_write);

// The trap→fiber bridge. While a fiber runs, the scheduler arms this per-thread hook; a trap
// (bounds, arithmetic, uncaught error) then calls it INSTEAD of ending the process — the hook
// switches control back to the scheduler, killing just that fiber (docs/design/fibers.md's
// crash isolation). It is NORETURN in effect (it switches away and never comes back). NULL on
// the root context and in every non-fiber program, where a trap stays fatal exactly as
// before. Defined in trap.c (always compiled) so a fiber-less build still links; only the
// fiber runtime ever sets it. NOT a stack swap via setjmp/longjmp — a cross-stack longjmp
// trips glibc's __longjmp_chk and side-steps the ASan fiber annotations — but the same
// neon_ctx_swap the normal fiber exit uses, so it is portable and ASan-clean.
extern _Thread_local void (*neon_fiber_trap_handler)(void);

// The drop for a heap string: the bytes live right after the header, so freeing the
// header frees both.
static inline void neon_str_drop(void* p) {
    neon_free(p);
}

// Allocate a fresh heap string holding `len` bytes copied from `src`.
static inline neon_str neon_str_new(const char* src, size_t len) {
    // A length with the top bit set is not a string, it is a negative number that has been
    // through a size_t — a corrupted or miscomputed slice bound. Trapping here names the
    // bug at its cheapest observable point, and as a bonus hands the optimizer the bound
    // its overflow analysis wants (this branch is what silences the -Wstringop-overflow
    // false positives that appeared when the fiber IO hooks made read lengths opaque).
    if (len > (SIZE_MAX >> 1)) {
        neon_trap("string length overflow");
    }
    // Explicit cast: `void*` converts implicitly in C but not in C++, and this header is
    // included from the C++ unit tests.
    neon_header* h = (neon_header*)neon_alloc(len, neon_str_drop);
    char* data = (char*)(h + 1);
    // Worth about 1.7% on word-frequency on its own, where this is reached once per token
    // to copy the four digits `neon_i64_to_string` just produced. Much less than the same
    // trick is worth in `neon_str_eq`, despite `memcpy` being the larger share of that
    // profile -- the copy sits beside an allocation that dwarfs it, while the compare sits
    // in a probe loop with nothing else in it. Profile share is not recoverable time.
    if (len <= NEON_STR_SHORT) {
        for (size_t i = 0; i < len; i++) {
            data[i] = src[i];
        }
    } else {
        memcpy(data, src, len);
    }
    neon_str s = {data, len, h};
    return s;
}

#endif
