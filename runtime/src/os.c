#ifndef _GNU_SOURCE
#define _GNU_SOURCE // sched_getaffinity, for neon_os_cpu_count
#endif

// The process interface: the arguments the OS handed the program, its environment, and
// the way out. `std::os` assembles these pieces — the natives hand over one value at a
// time because building a `List[str]` needs an element witness only compiled code has
// (the same reason `string::split` is Neon, not C).
//
// Text from the OS is bytes; a `str` is UTF-8 by decree, and this is exactly the IO
// boundary where the decree gets enforced. The policy is LOSSY: each invalid byte
// becomes U+FFFD. An arg or env var that is not UTF-8 is overwhelmingly a filename or
// junk, a throwing `args()` would poison every CLI's first line, and a trap would make
// a weird inherited environment fatal — replacement is the one option that keeps these
// functions total while never letting invalid bytes masquerade as a valid `str`.
//
// Valid argv bytes are handed over ZERO-COPY as immortal literals: argv lives exactly
// as long as the process, which is the lifetime `neon_str_lit` means. Env values are
// COPIED — `getenv`'s buffer has no such promise.

#include "libneon_rt.h"

#if defined(__linux__)
#include <sched.h>
#endif
#include <unistd.h>

#include "internal.h"
#include "platform.h"

#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int neon_os_argc_v = 0;
static char** neon_os_argv_v = NULL;

void neon_os_set_args(int argc, char** argv) {
    neon_os_argc_v = argc;
    neon_os_argv_v = argv;
}

int64_t neon_os_argc(void) {
    return (int64_t)neon_os_argc_v;
}

// One well-formed UTF-8 sequence at `s[i..n]`: its total length, or 0 when the bytes
// there are not one. Strict — overlong forms, surrogates and values past U+10FFFF are
// rejected, which is what makes the lossy conversion below trustworthy: everything it
// passes through really is `str`. The `lo`/`hi` bounds on the second byte are the whole
// of that strictness (RFC 3629's table); later bytes are plain continuations.
static size_t neon_utf8_seq(const unsigned char* s, size_t n, size_t i) {
    unsigned char b = s[i];
    if (b < 0x80) {
        return 1;
    }
    size_t need;
    unsigned char lo = 0x80, hi = 0xbf;
    if (b >= 0xc2 && b <= 0xdf) {
        need = 1;
    } else if (b == 0xe0) {
        need = 2;
        lo = 0xa0; // below A0 would be an overlong two-byte value
    } else if (b >= 0xe1 && b <= 0xec) {
        need = 2;
    } else if (b == 0xed) {
        need = 2;
        hi = 0x9f; // A0.. here is a UTF-16 surrogate, not a character
    } else if (b >= 0xee && b <= 0xef) {
        need = 2;
    } else if (b == 0xf0) {
        need = 3;
        lo = 0x90; // below 90 would be an overlong three-byte value
    } else if (b >= 0xf1 && b <= 0xf3) {
        need = 3;
    } else if (b == 0xf4) {
        need = 3;
        hi = 0x8f; // past 8F is beyond U+10FFFF
    } else {
        return 0; // C0/C1 (always overlong), F5.., or a bare continuation byte
    }
    if (n - i <= need) {
        return 0; // truncated
    }
    if (s[i + 1] < lo || s[i + 1] > hi) {
        return 0;
    }
    for (size_t k = 2; k <= need; k++) {
        if (s[i + k] < 0x80 || s[i + k] > 0xbf) {
            return 0;
        }
    }
    return need + 1;
}

// The boundary conversion: valid bytes pass through, each invalid byte becomes U+FFFD
// (EF BF BD). `immortal` chooses zero-copy for storage that lives as long as the
// process (argv); everything else is copied into an owned string. The valid case —
// nearly always — costs one walk and no allocation.
static neon_str neon_str_from_os(const char* p, size_t n, bool immortal) {
    const unsigned char* s = (const unsigned char*)p;
    size_t out_len = 0;
    bool valid = true;
    for (size_t i = 0; i < n;) {
        size_t seq = neon_utf8_seq(s, n, i);
        if (seq == 0) {
            valid = false;
            out_len += 3;
            i += 1;
        } else {
            out_len += seq;
            i += seq;
        }
    }
    if (valid) {
        return immortal ? neon_str_lit(p, n) : neon_str_new(p, n);
    }
    neon_header* h = (neon_header*)neon_alloc(out_len, neon_str_drop);
    char* data = (char*)(h + 1);
    size_t o = 0;
    for (size_t i = 0; i < n;) {
        size_t seq = neon_utf8_seq(s, n, i);
        if (seq == 0) {
            memcpy(data + o, "\xef\xbf\xbd", 3);
            o += 3;
            i += 1;
        } else {
            memcpy(data + o, p + i, seq);
            o += seq;
            i += seq;
        }
    }
    neon_str r = {data, out_len, h};
    return r;
}

// The boundary converter, exported: any subsystem receiving OS bytes (process output,
// most prominently) funnels them through the same lossy door argv and env use.
neon_str neon_os_lossy(const char* bytes, size_t len) {
    return neon_str_from_os(bytes, len, false);
}

neon_str neon_os_arg(int64_t i) {
    if (i < 0 || i >= (int64_t)neon_os_argc_v) {
        neon_trap_oob(i, (size_t)neon_os_argc_v);
    }
    const char* a = neon_os_argv_v[i];
    return neon_str_from_os(a, strlen(a), true);
}

// The name arrives as a `str` — not NUL-terminated — and `getenv` wants C. A fixed
// buffer, because environment names beyond a few hundred bytes are nobody's
// environment; an oversized one reads as unset rather than truncating into a
// DIFFERENT variable's value.
#define NEON_OS_NAME_MAX 512

static const char* neon_os_getenv(neon_str name) {
    size_t n = neon_str_len(&name);
    if (n >= NEON_OS_NAME_MAX || memchr(neon_str_data(&name), '\0', n) != NULL) {
        return NULL;
    }
    char buf[NEON_OS_NAME_MAX];
    memcpy(buf, neon_str_data(&name), n);
    buf[n] = '\0';
    return getenv(buf);
}

bool neon_os_has_env(neon_str name) {
    const char* v = neon_os_getenv(name);
    neon_str_release(name);
    return v != NULL;
}

neon_str neon_os_env_get(neon_str name) {
    const char* v = neon_os_getenv(name);
    neon_str_release(name);
    if (v == NULL) {
        // Guarded by `has_env` in the stdlib; an unset name still answers something
        // sane rather than reading NULL.
        return neon_str_lit("", 0);
    }
    return neon_str_from_os(v, strlen(v), false);
}

NEON_NORETURN void neon_os_exit(int64_t code) {
    // Like a trap: flush what the program said, then leave without teardown — the OS
    // reclaims memory, and `exit` is the program ending on purpose, not a fault, so
    // there is no message and no trace.
    fflush(stdout);
    fflush(stderr);
    neon_plat_exit_now((int)code);
}

// ---- stdin ----
//
// `io::read_line` and `io::read_all`, same lossy boundary as args and env. The raw
// line INCLUDES its trailing newline — the stdlib strips it — because "empty string"
// must keep meaning "end of input" and only the newline makes a blank line non-empty.

// Bytes from stdin into a growing buffer; `stop_at_newline` makes it a line read.
// Byte-at-a-time via `getc` is fine here: stdio buffers underneath, and stdin is not
// where a Neon program's inner loop lives. (Not POSIX `getline` — Windows lacks it.)
static neon_str neon_io_read_stdin(bool stop_at_newline) {
    size_t cap = 128, len = 0;
    char* buf = malloc(cap);
    if (buf == NULL) {
        neon_trap("out of memory");
    }
    int c;
    while ((c = getc(stdin)) != EOF) {
        if (len == cap) {
            cap *= 2;
            char* grown = realloc(buf, cap);
            if (grown == NULL) {
                neon_trap("out of memory");
            }
            buf = grown;
        }
        buf[len++] = (char)c;
        if (stop_at_newline && c == '\n') {
            break;
        }
    }
    neon_str s = neon_str_from_os(buf, len, false);
    free(buf);
    return s;
}

neon_str neon_io_read_line_raw(void) {
    return neon_io_read_stdin(true);
}

neon_str neon_io_read_all_stdin(void) {
    return neon_io_read_stdin(false);
}

// ---- runtime introspection (std::sys) ----
//
// What a program can ask about the runtime it is actually running on. Both answers are
// build-and-kernel facts, not preferences: whether this binary HAS the fiber machinery
// (x86-64 SysV Unix today — elsewhere the sources are not compiled and fiber calls would
// not link), and which non-blocking engine a fiber runtime would use here. The engine
// answer is honest about context: it reports what THIS seat opened when called inside a
// runtime, and what a runtime WOULD open when called outside one.

#if defined(NEON_RT_FIBERS)
#include "fiber_internal.h" // neon_sys_uring_here
#endif

bool neon_sys_has_fibers(void) {
#if defined(NEON_RT_FIBERS)
    return true;
#else
    return false;
#endif
}

// The atom a `std::sys::io_engine()` answers with, as its FNV-1a hash — atoms are hashes at
// runtime, and these must match what codegen computes for `:io_uring` / `:epoll` / `:none`.
static uint64_t neon_sys_atom(const char* s) {
    uint64_t h = 0xcbf29ce484222325ULL;
    for (const unsigned char* p = (const unsigned char*)s; *p != '\0'; p++) {
        h ^= *p;
        h *= 0x100000001b3ULL;
    }
    return h;
}

uint64_t neon_sys_io_engine(void) {
#if defined(NEON_RT_FIBERS)
    if (neon_sys_uring_here()) {
        return neon_sys_atom("io_uring");
    }
    return neon_sys_atom("epoll");
#else
    return neon_sys_atom("none");
#endif
}

// The number of hardware threads available to this process — what `fiber::runtime` sizes
// its scheduler to by default. The AFFINITY count, not the machine's, so a container or a
// `taskset` that restricts us is honoured rather than ignored (a scheduler with more seats
// than it may run on just adds contention). Falls back to the online count, then to 1: a
// wrong answer here should cost a little parallelism, never a failure to start.
int64_t neon_os_cpu_count(void) {
#if defined(__linux__)
    cpu_set_t set;
    CPU_ZERO(&set);
    if (sched_getaffinity(0, sizeof(set), &set) == 0) {
        int n = CPU_COUNT(&set);
        if (n > 0) {
            return n;
        }
    }
#endif
#if defined(_SC_NPROCESSORS_ONLN)
    long n = sysconf(_SC_NPROCESSORS_ONLN);
    if (n > 0) {
        return (int64_t)n;
    }
#endif
    return 1;
}
