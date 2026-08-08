// `dladdr` is a GNU extension on Linux; the define must precede the first glibc
// include in the translation unit or <dlfcn.h> hides it.
#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include "libneon_rt.h"

#include "platform.h"

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

// ---- stack traces ----
//
// Two captures serve two failure shapes. A TRAP (bounds, cast, arithmetic) is a fault
// at the fault site, so the trap prints the stack it is standing on. A thrown error is
// a VALUE — by the time nobody caught it and `neon_panic` runs, the interesting stack
// is long gone — so codegen calls `neon_trace_mark` at every `throw` and the panic
// prints the marked frames: where the error STARTED, not where it died.
//
// Printing is gated on NEON_BACKTRACE (any value but "0"), exactly Rust's shape: the
// capture is cheap and always runs, the sixty lines of output are opt-in. Symbol names
// need two things the default build omits — frame pointers to walk and a dynamic
// symbol table for `dladdr` — which is what `--stacktrace` builds add; without them
// the walk's sanity checks keep it safe and the trace degrades to raw addresses or
// fewer frames rather than to a crash.
//
// Under CBMC the whole mechanism compiles to nothing: a model walking symbolic frame
// pointers would explore every object in the program, and what a trap prints is not a
// property of anything (rule 4 in the models).

#define NEON_TRACE_MAX 64

#if !defined(__CPROVER__) && (defined(__linux__) || defined(__APPLE__)) && \
    (defined(__x86_64__) || defined(__aarch64__))
#define NEON_TRACE_SUPPORTED 1
#include <dlfcn.h>
#endif

static void* neon_trace_marked[NEON_TRACE_MAX];
static int neon_trace_marked_n = 0;

#ifdef NEON_TRACE_SUPPORTED
// Walk the frame-pointer chain from the caller. Every step is sanity-checked — the
// next frame must sit strictly above this one, within a sane distance, and aligned —
// because a build without `-fno-omit-frame-pointer` reuses the register and the chain
// dissolves into data; the checks turn "garbage chain" into "short trace".
static int neon_trace_walk(void** out, int max) {
    uintptr_t fp = (uintptr_t)__builtin_frame_address(0);
    int n = 0;
    while (fp != 0 && n < max) {
        if ((fp & (sizeof(void*) - 1)) != 0) break;
        void* ret = ((void**)fp)[1];
        uintptr_t next = (uintptr_t)((void**)fp)[0];
        if (ret == NULL) break;
        out[n++] = ret;
        if (next <= fp || next - fp > (1u << 22)) break;
        fp = next;
    }
    return n;
}

// `nl_std_u_ucollections_u_ulist_u_uset_Si64` -> `std::collections::list::set$i64`.
// The exact inverse of codegen's `mangle` (`backend/c.rs`): alnum verbatim, `_S` was
// `$`, `_u` was `_`, `_x%02x` a byte — and the IR spells module paths with literal
// `__`, shown here as `::`. Anything that is not an `nl_` symbol passes through.
static void neon_trace_demangle(const char* sym, char* out, size_t cap) {
    if (strncmp(sym, "nl_", 3) != 0) {
        snprintf(out, cap, "%s", sym);
        return;
    }
    char plain[256];
    size_t p = 0;
    for (const char* c = sym + 3; *c != '\0' && p + 1 < sizeof(plain);) {
        if (c[0] == '_' && c[1] == 'u') {
            plain[p++] = '_';
            c += 2;
        } else if (c[0] == '_' && c[1] == 'S') {
            plain[p++] = '$';
            c += 2;
        } else if (c[0] == '_' && c[1] == 'x' && c[2] != '\0' && c[3] != '\0') {
            char hex[3] = {c[2], c[3], '\0'};
            plain[p++] = (char)strtol(hex, NULL, 16);
            c += 4;
        } else {
            plain[p++] = *c++;
        }
    }
    plain[p] = '\0';
    // Module separators, best effort: the join is not injective for names that
    // themselves contain `__`, and a display string may be wrong in the way the
    // symbol already was.
    size_t o = 0;
    for (size_t i = 0; plain[i] != '\0' && o + 2 < cap;) {
        if (plain[i] == '_' && plain[i + 1] == '_') {
            out[o++] = ':';
            out[o++] = ':';
            i += 2;
        } else {
            out[o++] = plain[i++];
        }
    }
    out[o] = '\0';
}

static void neon_trace_print(void* const* addrs, int n) {
    for (int i = 0; i < n; i++) {
        Dl_info info;
        if (dladdr(addrs[i], &info) != 0 && info.dli_sname != NULL) {
            // The runtime's own frames say nothing about the user's program.
            if (strncmp(info.dli_sname, "neon_", 5) == 0) continue;
            char name[256];
            neon_trace_demangle(info.dli_sname, name, sizeof(name));
            fprintf(stderr, "   at %s\n", name);
            // The program's entry is the floor of every trace: `nl_main` is the
            // lowered `main`, and the C `main` beneath it is the same frame again
            // with the runtime's hat on.
            if (strcmp(info.dli_sname, "main") == 0 || strcmp(info.dli_sname, "nl_main") == 0) {
                break;
            }
        } else {
            fprintf(stderr, "   at %p\n", addrs[i]);
        }
    }
}

static int neon_trace_enabled(void) {
    const char* v = getenv("NEON_BACKTRACE");
    return v != NULL && strcmp(v, "0") != 0;
}
#endif

void neon_trace_mark(void) {
#ifdef NEON_TRACE_SUPPORTED
    neon_trace_marked_n = neon_trace_walk(neon_trace_marked, NEON_TRACE_MAX);
#endif
}

// The trace a dying process prints: the MARKED frames when a throw was the cause, the
// current stack otherwise — or the one-line hint when tracing is off.
static void neon_trace_report(int prefer_marked) {
#ifdef NEON_TRACE_SUPPORTED
    if (!neon_trace_enabled()) {
        fprintf(stderr, "  (set NEON_BACKTRACE=1 for a backtrace)\n");
        return;
    }
    if (prefer_marked && neon_trace_marked_n > 0) {
        fprintf(stderr, "  thrown from:\n");
        neon_trace_print(neon_trace_marked, neon_trace_marked_n);
        return;
    }
    void* here[NEON_TRACE_MAX];
    int n = neon_trace_walk(here, NEON_TRACE_MAX);
    neon_trace_print(here, n);
#else
    (void)prefer_marked;
#endif
}

// ---- traps ----
//
// A trap prints to stderr and exits immediately with `neon_plat_exit_now`: no atexit
// teardown, no unwind. The program is dying from a bug; the OS reclaims memory. Under
// NEON_DEBUG (a `-g` build) we abort() instead, so a debugger catches SIGABRT at the fault.

#define NEON_TRAP_CODE 101

NEON_NORETURN void neon_trap(const char* msg) {
    // Flush stdout first: exiting this way skips stdio teardown, and output the program
    // already produced before the fault (its golden up to this point) must still be seen.
    fflush(stdout);
    fprintf(stderr, "neon: %s\n", msg);
    neon_trace_report(0);
    fflush(stderr);
#ifdef NEON_DEBUG
    abort();
#else
    neon_plat_exit_now(NEON_TRAP_CODE);
#endif
}

NEON_NORETURN void neon_trap_oob(int64_t index, size_t len) {
    // The out-of-bounds trap, carrying the two numbers every report needs. Its own
    // entry point rather than callers formatting a message: a trap must not allocate,
    // and `fprintf` formats into stderr's own buffer. The stdlib's CATCHABLE
    // IndexError spells the same facts ("index 9 out of range for length 2"); a trap
    // saying less than the throw did was a debugging tax with no payer.
    fflush(stdout);
    fprintf(stderr, "neon: list index %lld out of range for length %zu\n",
            (long long)index, len);
    neon_trace_report(0);
    fflush(stderr);
#ifdef NEON_DEBUG
    abort();
#else
    neon_plat_exit_now(NEON_TRAP_CODE);
#endif
}

NEON_NORETURN void neon_panic(neon_str msg) {
    // Flush stdout first, for the same reason a trap does: exiting this way skips stdio
    // teardown, and whatever the program printed before failing must still be seen.
    fflush(stdout);
    fprintf(stderr, "neon: uncaught error: %.*s\n", (int)neon_str_len(&msg), neon_str_data(&msg));
    neon_trace_report(1);
    fflush(stderr);
    neon_plat_exit_now(NEON_TRAP_CODE);
}

NEON_NORETURN void neon_unreachable(void) {
    neon_trap("reached unreachable code");
}
