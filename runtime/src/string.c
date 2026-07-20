#include "libneon_rt.h"

#include "internal.h"

#include <stdio.h>

// ---- str ----

neon_str neon_str_lit(const char* data, size_t len) {
    neon_str s = {(char*)data, len, NULL}; // static: never freed
    return s;
}

bool neon_str_eq(neon_str a, neon_str b) {
    size_t n = neon_str_len(&a);
    return n == neon_str_len(&b) && memcmp(neon_str_data(&a), neon_str_data(&b), n) == 0;
}

// Byte-lexicographic order: the shared prefix decides, and if one string is a prefix of
// the other the shorter sorts first. `memcmp`'s sign is only guaranteed meaningful over
// the common length, hence comparing lengths separately rather than over the longer one.
// This is bytes, not codepoints and not collation -- `byte_len`'s naming rule applies.
int neon_str_cmp(neon_str a, neon_str b) {
    size_t la = neon_str_len(&a), lb = neon_str_len(&b);
    size_t n = la < lb ? la : lb;
    int c = n ? memcmp(neon_str_data(&a), neon_str_data(&b), n) : 0;
    if (c != 0) {
        return c < 0 ? -1 : 1;
    }
    return la < lb ? -1 : (la > lb ? 1 : 0);
}

neon_str neon_str_concat(neon_str a, neon_str b) {
    size_t la = neon_str_len(&a), lb = neon_str_len(&b);
    neon_header* h = neon_alloc(la + lb, neon_str_drop);
    char* data = (char*)(h + 1);
    memcpy(data, neon_str_data(&a), la);
    memcpy(data + la, neon_str_data(&b), lb);
    neon_str s = {data, la + lb, h};
    neon_str_release(a);
    neon_str_release(b);
    return s;
}

// The `+` operator. It borrows both operands -- the IR treats a `prim.add`'s inputs as
// borrowed and releases them itself at their last use -- so this must not release them.
neon_str neon_str_add(neon_str a, neon_str b) {
    size_t la = neon_str_len(&a), lb = neon_str_len(&b);
    neon_header* h = neon_alloc(la + lb, neon_str_drop);
    char* data = (char*)(h + 1);
    memcpy(data, neon_str_data(&a), la);
    memcpy(data + la, neon_str_data(&b), lb);
    neon_str s = {data, la + lb, h};
    return s;
}

// ---- string natives (consume their str arguments) ----

// The byte offset of `needle` in `hay`, or -1. An empty needle is found at 0.
static int64_t str_index_of(neon_str hay, neon_str needle) {
    size_t nl = neon_str_len(&needle), hl = neon_str_len(&hay);
    if (nl == 0) return 0;
    if (nl > hl) return -1;
    const char* h = neon_str_data(&hay);
    const char* n = neon_str_data(&needle);
    for (size_t i = 0; i + nl <= hl; i++) {
        if (memcmp(h + i, n, nl) == 0) return (int64_t)i;
    }
    return -1;
}

int64_t neon_str_byte_len(neon_str s) {
    int64_t r = (int64_t)neon_str_len(&s);
    neon_str_release(s);
    return r;
}

bool neon_str_is_empty(neon_str s) {
    bool r = neon_str_len(&s) == 0;
    neon_str_release(s);
    return r;
}

neon_str neon_str_to_upper(neon_str s) {
    neon_str r = neon_str_new(neon_str_data(&s), neon_str_len(&s));
    // `r` was just allocated here and is not shared, so writing through it is sound. The
    // pointer is re-derived from `&r` rather than cached across the loop for the sake of
    // the reader: under SSO it points inside `r` itself.
    char* w = neon_str_data_mut(&r);
    for (size_t i = 0; i < neon_str_len(&r); i++) {
        char c = w[i];
        if (c >= 'a' && c <= 'z') w[i] = (char)(c - 32);
    }
    neon_str_release(s);
    return r;
}

neon_str neon_str_to_lower(neon_str s) {
    neon_str r = neon_str_new(neon_str_data(&s), neon_str_len(&s));
    // `r` was just allocated here and is not shared, so writing through it is sound. The
    // pointer is re-derived from `&r` rather than cached across the loop for the sake of
    // the reader: under SSO it points inside `r` itself.
    char* w = neon_str_data_mut(&r);
    for (size_t i = 0; i < neon_str_len(&r); i++) {
        char c = w[i];
        if (c >= 'A' && c <= 'Z') w[i] = (char)(c + 32);
    }
    neon_str_release(s);
    return r;
}

neon_str neon_str_repeat(neon_str s, int64_t n) {
    if (n <= 0) {
        neon_str_release(s);
        return neon_str_lit("", 0);
    }
    size_t len = neon_str_len(&s), total = len * (size_t)n;
    neon_header* h = neon_alloc(total, neon_str_drop);
    char* data = (char*)(h + 1);
    for (int64_t i = 0; i < n; i++) memcpy(data + (size_t)i * len, neon_str_data(&s), len);
    neon_str r = {data, total, h};
    neon_str_release(s);
    return r;
}

bool neon_str_contains(neon_str s, neon_str needle) {
    bool r = str_index_of(s, needle) >= 0;
    neon_str_release(s);
    neon_str_release(needle);
    return r;
}

bool neon_str_starts_with(neon_str s, neon_str prefix) {
    size_t pl = neon_str_len(&prefix);
    bool r = pl <= neon_str_len(&s)
             && memcmp(neon_str_data(&s), neon_str_data(&prefix), pl) == 0;
    neon_str_release(s);
    neon_str_release(prefix);
    return r;
}

bool neon_str_ends_with(neon_str s, neon_str suffix) {
    size_t sl = neon_str_len(&s), fl = neon_str_len(&suffix);
    bool r = fl <= sl
             && memcmp(neon_str_data(&s) + sl - fl, neon_str_data(&suffix), fl) == 0;
    neon_str_release(s);
    neon_str_release(suffix);
    return r;
}

// A byte slice: `str` is byte-indexed throughout (`byte_len`, `find`), so this cuts at
// byte offsets and may split a UTF-8 sequence — the caller asked for bytes.
neon_str neon_str_slice_unchecked(neon_str s, int64_t from, int64_t to) {
    neon_str r = neon_str_new(neon_str_data(&s) + from, (size_t)(to - from));
    neon_str_release(s);
    return r;
}

// The single byte at `i`. `str` is byte-indexed throughout, so this indexes bytes and may
// land inside a UTF-8 sequence — the same contract as `slice` and `find`.
neon_str neon_str_char_at_unchecked(neon_str s, int64_t i) {
    neon_str r = neon_str_new(neon_str_data(&s) + i, 1);
    neon_str_release(s);
    return r;
}

int64_t neon_str_index_of(neon_str s, neon_str needle) {
    int64_t r = str_index_of(s, needle);
    neon_str_release(s);
    neon_str_release(needle);
    return r;
}

// Whether the whole string is a decimal integer, optionally signed. Kept separate from
// parsing so the Neon wrapper decides what to throw.
bool neon_str_is_int(neon_str s) {
    size_t len = neon_str_len(&s), i = 0;
    const char* d = neon_str_data(&s);
    if (len > 0 && (d[0] == '-' || d[0] == '+')) i = 1;
    bool any = false;
    for (; i < len; i++) {
        if (d[i] < '0' || d[i] > '9') {
            neon_str_release(s);
            return false;
        }
        any = true;
    }
    neon_str_release(s);
    return any;
}

int64_t neon_str_parse_int(neon_str s) {
    int64_t sign = 1, v = 0;
    size_t len = neon_str_len(&s), i = 0;
    const char* d = neon_str_data(&s);
    if (len > 0 && (d[0] == '-' || d[0] == '+')) {
        sign = d[0] == '-' ? -1 : 1;
        i = 1;
    }
    for (; i < len; i++) {
        v = (int64_t)((uint64_t)v * 10 + (uint64_t)(d[i] - '0'));
    }
    neon_str_release(s);
    return (int64_t)((uint64_t)v * (uint64_t)sign);
}

// ---- to-string natives ----

// Hand-rolled rather than `snprintf("%lld")`, because this is hot: on the word-frequency
// benchmark, where every counted token is interpolated into a string, digit formatting was
// ~40% of the run. `snprintf` re-parses its format string and walks its full conversion
// machinery on every call to reach the same digit loop written out below.
//
// The longest result is `INT64_MIN` -- "-9223372036854775808", 20 characters. `neon_str`
// carries its length and is not NUL-terminated, so 20 is exact rather than generous.
neon_str neon_i64_to_string(int64_t n) {
    char buf[20];
    char* end = buf + sizeof buf;
    char* p = end;

    // Negate through `uint64_t`. `-INT64_MIN` overflows `int64_t` and is undefined, but
    // unsigned negation is defined as modular and lands on 9223372036854775808 exactly --
    // which is `INT64_MIN`'s magnitude, the one value a naive `-n` gets wrong.
    uint64_t u = n < 0 ? 0u - (uint64_t)n : (uint64_t)n;

    // Digits emerge least-significant first, so fill the buffer from the right. `do`/`while`
    // rather than `while`, so that n == 0 writes its "0" instead of an empty string.
    do {
        *--p = (char)('0' + u % 10);
        u /= 10;
    } while (u);

    if (n < 0) *--p = '-';

    return neon_str_new(p, (size_t)(end - p));
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

// `join` builds a string out of a `List[str]`, so it lives with the other string
// constructors rather than with the list natives -- it is the only list-taking function
// that allocates a `neon_str`.
neon_str neon_str_join(neon_list* parts, neon_str sep) {
    // Borrowed, not copied: the elements stay in the list's buffer, which outlives this
    // function, so `neon_str_data` on one of them is sound for as long as it is used here.
    const neon_str* items = (const neon_str*)parts->data;
    size_t seplen = neon_str_len(&sep);
    size_t total = 0;
    for (size_t i = 0; i < parts->len; i++) {
        total += neon_str_len(&items[i]);
    }
    if (parts->len > 1) total += seplen * (parts->len - 1);

    neon_header* h = neon_alloc(total, neon_str_drop);
    char* data = (char*)(h + 1);
    size_t off = 0;
    for (size_t i = 0; i < parts->len; i++) {
        if (i > 0) {
            memcpy(data + off, neon_str_data(&sep), seplen);
            off += seplen;
        }
        size_t elen = neon_str_len(&items[i]);
        memcpy(data + off, neon_str_data(&items[i]), elen);
        off += elen;
    }
    neon_str s = {data, total, h};
    neon_release((neon_header*)parts); // consumes parts (drops its str elements)
    neon_str_release(sep);
    return s;
}
