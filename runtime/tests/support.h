// Shared test support: the element/key witnesses the container tests build lists and maps
// with, plus a couple of string helpers.
//
// The runtime is compiled *into* the test binary (see `CMakeLists.txt`), so these tests can
// reach `src/internal.h` and the non-exported helpers, and AddressSanitizer instruments the
// runtime itself rather than only the test code.

#ifndef NEON_RT_TEST_SUPPORT_H
#define NEON_RT_TEST_SUPPORT_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "libneon_rt.h"
#include "internal.h"

// ---- the platform seam the tests need ----
//
// Smaller than the runtime's (`src/platform.h`) and separate from it on purpose: nothing
// here is anything the runtime does, and a test-only need has no business widening the
// seam a shipped archive is built through.
//
// Only two things are behind it. `getpid` is spelled `_getpid` by a Windows CRT, and the
// descriptor calls the stream capture in `io_test.c` needs are spelled with a leading
// underscore there too. Both are the same call under a different name -- if something ever
// needs a genuinely *different* call, it belongs in a function here, not a `#define`.
#ifdef _WIN32
#include <io.h>
#include <process.h>
#define nt_getpid _getpid
#define nt_dup _dup
#define nt_dup2 _dup2
#define nt_fileno _fileno
#define nt_close _close
#else
#include <unistd.h>
#define nt_getpid getpid
#define nt_dup dup
#define nt_dup2 dup2
#define nt_fileno fileno
#define nt_close close
#endif

// Enough for any path these tests build. They are made, not discovered -- see
// `nt_temp_path` -- so this is a bound on a name this file chose, not on what the platform
// might hand back.
#define NT_PATH_MAX 64

// A fresh, existing, empty file for a test to work against, named into `path` (capacity
// `NT_PATH_MAX`). The caller owns removing it.
//
// In the working directory rather than a system temp directory, and built rather than
// requested. `mkstemp` is POSIX-only, and its Windows counterpart (`GetTempFileNameA`)
// returns a path in the *ANSI* code page while `neon_io_open` decodes UTF-8 -- so on a
// machine whose temp directory is not ASCII the two would disagree about which file the
// test meant. A name this file composes out of ASCII cannot have that problem. ctest runs
// in the build directory, which is writable and is where a leftover belongs anyway.
//
// The pid keeps two concurrent runs of the suite apart; the counter keeps two files within
// one test apart. `tag` is there to make a leftover say which test dropped it.
static inline void nt_temp_path(char* path, const char* tag) {
    static unsigned nt_temp_seq = 0;
    snprintf(path, NT_PATH_MAX, "neon_rt_%s_%ld_%u.tmp", tag, (long)nt_getpid(),
             nt_temp_seq++);
    // Created, not just named: `mkstemp` left an existing empty file behind and the tests
    // that open for reading depend on that. "wb" and not "w" -- a Windows text-mode stream
    // would rewrite every newline that later reaches this file.
    FILE* f = fopen(path, "wb");
    if (f != NULL) {
        fclose(f);
    }
}

// ---- witnesses ----
//
// A container carries a value-witness describing its element type: its size and how to
// retain/release/compare one. A scalar element (an `i64`) holds nothing counted, so its
// retain/release are NULL. A `str` element owns a heap allocation, so its retain/release
// forward to the string's refcount.

static inline bool nt_i64_eq(const void* a, const void* b) {
    return *(const int64_t*)a == *(const int64_t*)b;
}
static inline int nt_i64_cmp(const void* a, const void* b) {
    int64_t x = *(const int64_t*)a, y = *(const int64_t*)b;
    return x < y ? -1 : (x > y ? 1 : 0);
}
static const neon_witness nt_i64_w = {sizeof(int64_t), NULL, NULL, nt_i64_eq, nt_i64_cmp};

static inline void nt_str_retain(void* p) { neon_str_retain(*(neon_str*)p); }
static inline void nt_str_release(void* p) { neon_str_release(*(neon_str*)p); }
static inline bool nt_str_eq(const void* a, const void* b) {
    return neon_str_eq(*(const neon_str*)a, *(const neon_str*)b);
}
static inline int nt_str_cmp(const void* a, const void* b) {
    return neon_str_cmp(*(const neon_str*)a, *(const neon_str*)b);
}
static const neon_witness nt_str_w = {sizeof(neon_str), nt_str_retain, nt_str_release,
                                      nt_str_eq, nt_str_cmp};

// A `str` used as a map key: content-hashed and content-compared.
static inline uint64_t nt_str_hash(const void* p) {
    const neon_str* s = (const neon_str*)p;
    return neon_hash_bytes(neon_str_data(s), neon_str_len(s));
}
static const neon_key_witness nt_str_key = {&nt_str_w, nt_str_hash, nt_str_eq};

// An `i64` used as a map key.
static inline uint64_t nt_i64_hash(const void* p) {
    return neon_hash_bytes(p, sizeof(int64_t));
}
static const neon_key_witness nt_i64_key = {&nt_i64_w, nt_i64_hash, nt_i64_eq};

// A heap string with a non-NULL owner, so refcount behaviour is exercised (a literal, with
// owner == NULL, makes retain/release no-ops and hides it).
static inline neon_str nt_owned(const char* s) {
    return neon_str_new(s, strlen(s));
}

// Compare a `neon_str`'s bytes against a C string.
static inline bool nt_str_is(neon_str s, const char* expected) {
    size_t n = neon_str_len(&s);
    return n == strlen(expected) && memcmp(neon_str_data(&s), expected, n) == 0;
}


// A per-holder channel handle, minted the way the language mints one: a `Channel[T]` value
// is an arena-resident REF owning a reference to the shared body, so each holder needs its
// own (sharing one ref across fibers would share a thread-local count). Every channel op
// consumes its handle, so each call takes a fresh one. `neon_wcopy_channel` is exactly the
// copy the compiler emits when a channel crosses a fiber boundary.
static inline neon_channel_ref* nt_chan_handle(neon_channel_ref* const* src) {
    neon_channel_ref* mine;
    neon_wcopy_channel(src, &mine);
    return mine;
}
#endif
