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

#include "libneon_rt.h"
#include "internal.h"

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
    return neon_str_new(s, __builtin_strlen(s));
}

// Compare a `neon_str`'s bytes against a C string.
static inline bool nt_str_is(neon_str s, const char* expected) {
    size_t n = neon_str_len(&s);
    return n == __builtin_strlen(expected) && __builtin_memcmp(neon_str_data(&s), expected, n) == 0;
}

#endif
