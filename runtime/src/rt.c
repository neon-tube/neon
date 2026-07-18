#include "rt.h"

#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

// ---- lifecycle ----

void neon_rt_init(void) {
    // Nothing yet; a hook for allocator/setup once there is any.
}

void neon_retain(neon_header* h) {
    if (h == NULL || (h->flags & NEON_IMMORTAL)) {
        return;
    }
    h->rc++;
}

void neon_release(neon_header* h) {
    if (h == NULL || (h->flags & NEON_IMMORTAL)) {
        return;
    }
    if (--h->rc == 0) {
        h->drop(h);
    }
}

void* neon_alloc(size_t bytes, void (*drop)(void*)) {
    neon_header* h = malloc(sizeof(neon_header) + bytes);
    if (h == NULL) {
        neon_trap("out of memory");
    }
    h->rc = 1;
    h->flags = 0;
    h->drop = drop;
    return h;
}

void neon_free(void* p) {
    free(p);
}

// ---- traps ----
//
// A trap prints to stderr and exits immediately with _exit: no atexit teardown, no
// unwind. The program is dying from a bug; the OS reclaims memory.

#define NEON_TRAP_CODE 134

_Noreturn void neon_trap(const char* msg) {
    fprintf(stderr, "neon: %s\n", msg);
    fflush(stderr);
    _exit(NEON_TRAP_CODE);
}

_Noreturn void neon_panic(neon_str msg) {
    fprintf(stderr, "neon: uncaught error: %.*s\n", (int)msg.len, msg.data);
    fflush(stderr);
    _exit(1);
}

_Noreturn void neon_unreachable(void) {
    neon_trap("reached unreachable code");
}

// ---- trapping i64 arithmetic ----

int64_t neon_i64_add(int64_t a, int64_t b) {
    int64_t r;
    if (__builtin_add_overflow(a, b, &r)) {
        neon_trap("integer overflow");
    }
    return r;
}

int64_t neon_i64_sub(int64_t a, int64_t b) {
    int64_t r;
    if (__builtin_sub_overflow(a, b, &r)) {
        neon_trap("integer overflow");
    }
    return r;
}

int64_t neon_i64_mul(int64_t a, int64_t b) {
    int64_t r;
    if (__builtin_mul_overflow(a, b, &r)) {
        neon_trap("integer overflow");
    }
    return r;
}

int64_t neon_i64_div(int64_t a, int64_t b) {
    if (b == 0) {
        neon_trap("division by zero");
    }
    if (a == INT64_MIN && b == -1) {
        neon_trap("integer overflow");
    }
    return a / b;
}

int64_t neon_i64_rem(int64_t a, int64_t b) {
    if (b == 0) {
        neon_trap("division by zero");
    }
    if (a == INT64_MIN && b == -1) {
        return 0;
    }
    return a % b;
}

int64_t neon_i64_neg(int64_t a) {
    if (a == INT64_MIN) {
        neon_trap("integer overflow");
    }
    return -a;
}

// ---- str ----

// The drop for a heap string: the bytes live right after the header, so freeing the
// header frees both.
static void neon_str_drop(void* p) {
    neon_free(p);
}

// Allocate a fresh heap string holding `len` bytes copied from `src`.
static neon_str neon_str_new(const char* src, size_t len) {
    neon_header* h = neon_alloc(len, neon_str_drop);
    char* data = (char*)(h + 1);
    if (len) {
        memcpy(data, src, len);
    }
    neon_str s = {data, len, h};
    return s;
}

neon_str neon_str_lit(const char* data, size_t len) {
    neon_str s = {(char*)data, len, NULL}; // static: never freed
    return s;
}

bool neon_str_eq(neon_str a, neon_str b) {
    return a.len == b.len && memcmp(a.data, b.data, a.len) == 0;
}

neon_str neon_str_concat(neon_str a, neon_str b) {
    neon_header* h = neon_alloc(a.len + b.len, neon_str_drop);
    char* data = (char*)(h + 1);
    memcpy(data, a.data, a.len);
    memcpy(data + a.len, b.data, b.len);
    neon_str s = {data, a.len + b.len, h};
    neon_release(a.owner);
    neon_release(b.owner);
    return s;
}

// ---- to-string natives ----

neon_str neon_i64_to_string(int64_t n) {
    char buf[24];
    int len = snprintf(buf, sizeof buf, "%lld", (long long)n);
    return neon_str_new(buf, (size_t)len);
}

neon_str neon_f64_to_string(double x) {
    char buf[32];
    int len = snprintf(buf, sizeof buf, "%g", x);
    return neon_str_new(buf, (size_t)len);
}

neon_str neon_bool_to_string(bool b) {
    return neon_str_lit(b ? "true" : "false", b ? 4 : 5);
}

neon_str neon_str_to_string(neon_str s) {
    return s; // identity; ownership passes through
}

// ---- io ----

void neon_io_println(neon_str s) {
    fwrite(s.data, 1, s.len, stdout);
    fputc('\n', stdout);
    neon_release(s.owner); // consumes s
}
