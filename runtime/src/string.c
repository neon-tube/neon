#include "libneon_rt.h"

#include "internal.h"

#include <stdio.h>
#include <stdlib.h>

// ---- str ----

neon_str neon_str_lit(const char* data, size_t len) {
    neon_str s = {(char*)data, len, NULL}; // static: never freed
    return s;
}

bool neon_str_eq(neon_str a, neon_str b) {
    size_t n = neon_str_len(&a);
    if (n != neon_str_len(&b)) {
        return false;
    }
    const char* pa = neon_str_data(&a);
    const char* pb = neon_str_data(&b);
    // The short path is the map-key path: every probe in `neon_map_slot` lands here, and on
    // the word-frequency benchmark this call was 16.5% of the run to compare five bytes.
    // See `NEON_STR_SHORT` for why the boundary is where it is.
    if (n <= NEON_STR_SHORT) {
        for (size_t i = 0; i < n; i++) {
            if (pa[i] != pb[i]) {
                return false;
            }
        }
        return true;
    }
    return memcmp(pa, pb, n) == 0;
}

// Byte-lexicographic order: the shared prefix decides, and if one string is a prefix of
// the other the shorter sorts first. `memcmp`'s sign is only guaranteed meaningful over
// the common length, hence comparing lengths separately rather than over the longer one.
// This is bytes, not codepoints and not collation -- `byte_len`'s naming rule applies.
int neon_str_cmp(neon_str a, neon_str b) {
    size_t la = neon_str_len(&a), lb = neon_str_len(&b);
    size_t n = la < lb ? la : lb;
    const char* pa = neon_str_data(&a);
    const char* pb = neon_str_data(&b);
    // Same short-length fast path as `eq`, and the same reason. The three-way result is
    // built from the first differing byte rather than from `memcmp`'s sign: compared as
    // `unsigned char`, because plain `char` is signed here and a byte over 127 would
    // otherwise order *before* an ASCII one and disagree with `memcmp` on UTF-8.
    int c = 0;
    if (n <= NEON_STR_SHORT) {
        for (size_t i = 0; i < n; i++) {
            if (pa[i] != pb[i]) {
                c = (unsigned char)pa[i] < (unsigned char)pb[i] ? -1 : 1;
                break;
            }
        }
    } else {
        c = memcmp(pa, pb, n);
    }
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

// A one-byte string from a byte value (0..=255, masked). The low-level inverse of
// `byte_at`: a decoder that has assembled a UTF-8 byte from an escape emits it with this,
// and reassembling a character byte-by-byte is how `str`-as-bytes stays honest.
neon_str neon_str_from_byte(int64_t b) {
    char c = (char)(unsigned char)(b & 0xFF);
    return neon_str_new(&c, 1);
}

// The same byte as `neon_str_char_at_unchecked`, as its numeric value rather than as a
// one-byte string. Unsigned, so a byte above 0x7F reads as 128..=255 rather than as a
// negative — the caller is classifying bytes, and a sign here would make every non-ASCII
// byte compare below every ASCII one.
int64_t neon_str_byte_at_unchecked(neon_str s, int64_t i) {
    int64_t b = (unsigned char)neon_str_data(&s)[i];
    neon_str_release(s);
    return b;
}

int64_t neon_str_index_of(neon_str s, neon_str needle) {
    int64_t r = str_index_of(s, needle);
    neon_str_release(s);
    neon_str_release(needle);
    return r;
}

// ---- codepoints (UTF-8 decode) ----
//
// The one decoder every `codepoint_*` native shares, so they can never disagree about where
// a scalar ends or whether it is valid. It decodes the unit beginning at `d[off]` (with
// `off < len` assumed) and returns the number of bytes it spans. On a well-formed scalar
// `*valid` is true and `*cp` is the scalar value; on ANY malformation — a stray
// continuation byte, a truncated tail, an overlong encoding, a surrogate, or a value past
// U+10FFFF — it consumes exactly ONE byte, sets `*valid` false and `*cp` to U+FFFD. That
// one-byte-at-a-time recovery is the whole replacement contract in a single place: it always
// makes progress and is fully deterministic, so `codepoint_len` counts and `codepoints`
// emits the same units `is_valid_utf8` judges.
static size_t utf8_decode(const unsigned char* d, size_t len, size_t off, uint32_t* cp,
                          bool* valid) {
    unsigned char b0 = d[off];
    if (b0 < 0x80) {
        *cp = b0;
        *valid = true;
        return 1;
    }
    size_t need;
    uint32_t c, min;
    if ((b0 & 0xE0) == 0xC0) {
        need = 1;
        c = b0 & 0x1Fu;
        min = 0x80;
    } else if ((b0 & 0xF0) == 0xE0) {
        need = 2;
        c = b0 & 0x0Fu;
        min = 0x800;
    } else if ((b0 & 0xF8) == 0xF0) {
        need = 3;
        c = b0 & 0x07u;
        min = 0x10000;
    } else {
        // A continuation byte (0x80..0xBF) or a 5/6-byte lead (0xF8..0xFF) begins nothing.
        *cp = 0xFFFD;
        *valid = false;
        return 1;
    }
    if (off + 1 + need > len) { // truncated: the tail runs past the end of the string
        *cp = 0xFFFD;
        *valid = false;
        return 1;
    }
    for (size_t k = 1; k <= need; k++) {
        unsigned char bk = d[off + k];
        if ((bk & 0xC0) != 0x80) { // a byte that is not a continuation cuts the sequence short
            *cp = 0xFFFD;
            *valid = false;
            return 1;
        }
        c = (c << 6) | (uint32_t)(bk & 0x3Fu);
    }
    if (c < min || c > 0x10FFFF || (c >= 0xD800 && c <= 0xDFFF)) { // overlong, out of range, surrogate
        *cp = 0xFFFD;
        *valid = false;
        return 1;
    }
    *cp = c;
    *valid = true;
    return 1 + need;
}

// The number of Unicode scalar values, decoding by the replacement rule so a byte string
// that is not UTF-8 still gets a definite count rather than an error — one per scalar, plus
// one per malformed byte. `byte_len` is the byte twin; the two agree only on ASCII.
int64_t neon_str_codepoint_len(neon_str s) {
    const unsigned char* d = (const unsigned char*)neon_str_data(&s);
    size_t len = neon_str_len(&s), i = 0;
    int64_t n = 0;
    while (i < len) {
        uint32_t cp;
        bool valid;
        i += utf8_decode(d, len, i, &cp, &valid);
        n++;
    }
    neon_str_release(s);
    return n;
}

// Strict validity: every unit decodes clean, with no replacement anywhere. This is the
// honest check the replacement-tolerant `codepoint_*` functions deliberately do not make, so
// a caller that must reject non-UTF-8 input asks here.
bool neon_str_is_valid_utf8(neon_str s) {
    const unsigned char* d = (const unsigned char*)neon_str_data(&s);
    size_t len = neon_str_len(&s), i = 0;
    bool ok = true;
    while (i < len) {
        uint32_t cp;
        bool valid;
        i += utf8_decode(d, len, i, &cp, &valid);
        if (!valid) {
            ok = false;
            break;
        }
    }
    neon_str_release(s);
    return ok;
}

// The byte width of the scalar at byte offset `off` (1..=4, or 1 for a malformed unit).
// Paired with `codepoint_here`, this is the advance that keeps `codepoints()` a single
// linear pass: the loop emits the scalar here and steps `off` on by this.
int64_t neon_str_utf8_seq_len(neon_str s, int64_t off) {
    const unsigned char* d = (const unsigned char*)neon_str_data(&s);
    size_t len = neon_str_len(&s);
    uint32_t cp;
    bool valid;
    size_t w = utf8_decode(d, len, (size_t)off, &cp, &valid);
    neon_str_release(s);
    return (int64_t)w;
}

// The scalar at byte offset `off` as a fresh one-codepoint `str`. A well-formed scalar comes
// back as its own bytes verbatim — so `codepoints` joined back together reproduces valid
// input exactly — and a malformed unit comes back as U+FFFD, the replacement character, the
// same substitution `codepoint_len` counted.
neon_str neon_str_codepoint_here(neon_str s, int64_t off) {
    const unsigned char* d = (const unsigned char*)neon_str_data(&s);
    size_t len = neon_str_len(&s);
    uint32_t cp;
    bool valid;
    size_t w = utf8_decode(d, len, (size_t)off, &cp, &valid);
    neon_str r;
    if (valid) {
        r = neon_str_new((const char*)d + off, w);
    } else {
        static const char repl[3] = {(char)0xEF, (char)0xBF, (char)0xBD}; // U+FFFD in UTF-8
        r = neon_str_new(repl, sizeof repl);
    }
    neon_str_release(s);
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

// The float twins of `is_int`/`parse_int`, same split for the same reason: the check and
// the parse are two natives so the Neon wrapper owns the error. Both delegate to
// `strtod`, which is what defines "a float" here — decimal or scientific notation, `inf`
// and `nan` spellings included — with two adjustments: the whole string must be
// consumed, and leading whitespace (which `strtod` skips) is rejected, matching
// `to_int`'s "no trimming for you". A `neon_str` is not NUL-terminated, so the bytes are
// copied to a bounded buffer first; anything longer than the buffer is not a number
// anyone wrote by hand, and reads as unparseable rather than truncating.
#define NEON_FLOAT_MAX 64

static bool str_to_double(neon_str* s, double* out) {
    size_t len = neon_str_len(s);
    const char* d = neon_str_data(s);
    if (len == 0 || len >= NEON_FLOAT_MAX) {
        return false;
    }
    if (d[0] == ' ' || d[0] == '\t' || d[0] == '\n' || d[0] == '\r') {
        return false;
    }
    char buf[NEON_FLOAT_MAX];
    memcpy(buf, d, len);
    buf[len] = '\0';
    char* end = NULL;
    double v = strtod(buf, &end);
    if (end != buf + len) {
        return false;
    }
    *out = v;
    return true;
}

bool neon_str_is_float(neon_str s) {
    double v;
    bool ok = str_to_double(&s, &v);
    neon_str_release(s);
    return ok;
}

double neon_str_parse_float(neon_str s) {
    double v = 0.0;
    (void)str_to_double(&s, &v);
    neon_str_release(s);
    return v;
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

// ---- the send copy (witness `copy`) ----
//
// Deep-relocate one `neon_str` slot: a literal (owner NULL, static bytes) is its own
// independent copy; an owned string gets fresh bytes through `neon_str_new`, whose
// allocation lands wherever the AMBIENT routing points — the shared heap, when this runs
// under a channel send. See the `copy` member's contract in neon/core.h.
void neon_wcopy_str(const void* src, void* dst) {
    const neon_str* s = (const neon_str*)src;
    if (s->owner == NULL) {
        *(neon_str*)dst = *s;
        return;
    }
    *(neon_str*)dst = neon_str_new(s->data, s->len);
}
