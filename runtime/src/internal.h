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
#if defined(__GNUC__) && !defined(NEON_CBMC)
#  define NEON_TLS_IE __attribute__((tls_model("initial-exec")))
#else
#  define NEON_TLS_IE
#endif
extern _Thread_local NEON_TLS_IE neon_arena* neon_current_arena;

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
