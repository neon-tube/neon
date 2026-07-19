#include "rt.h"

#include <stdio.h>
#include <assert.h>
#include <errno.h>
#include <limits.h>
#include <fcntl.h>
#include <sys/uio.h>

// The batch size for `writev`. `IOV_MAX` is only visible under feature-test macros we do
// not set, and a *smaller* batch is always valid -- the call just runs more than once -- so
// this pins the value POSIX guarantees Linux provides rather than probing for it.
#define NEON_IOV_MAX 1024
#include <math.h>
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
// unwind. The program is dying from a bug; the OS reclaims memory. Under NEON_DEBUG
// (a `-g` build) we abort() instead, so a debugger catches SIGABRT at the fault.

#define NEON_TRAP_CODE 101

_Noreturn void neon_trap(const char* msg) {
    // Flush stdout first: `_exit` skips stdio teardown, and output the program already
    // produced before the fault (its golden up to this point) must still be seen.
    fflush(stdout);
    fprintf(stderr, "neon: %s\n", msg);
    fflush(stderr);
#ifdef NEON_DEBUG
    abort();
#else
    _exit(NEON_TRAP_CODE);
#endif
}

_Noreturn void neon_panic(neon_str msg) {
    // Flush stdout first, for the same reason a trap does: `_exit` skips stdio teardown,
    // and whatever the program printed before failing must still be seen.
    fflush(stdout);
    fprintf(stderr, "neon: uncaught error: %.*s\n", (int)msg.len, msg.data);
    fflush(stderr);
    _exit(NEON_TRAP_CODE);
}

_Noreturn void neon_unreachable(void) {
    neon_trap("reached unreachable code");
}

// ---- i64 arithmetic ----
//
// `+`, `-`, `*`, and unary `-` wrap on overflow (two's complement, no trap); the
// unsigned round-trip is how C gives that defined behaviour rather than UB. Division and
// remainder trap on a zero divisor and on INT64_MIN / -1, whose true quotient is not
// representable.

int64_t neon_i64_add(int64_t a, int64_t b) {
    return (int64_t)((uint64_t)a + (uint64_t)b);
}

int64_t neon_i64_sub(int64_t a, int64_t b) {
    return (int64_t)((uint64_t)a - (uint64_t)b);
}

int64_t neon_i64_mul(int64_t a, int64_t b) {
    return (int64_t)((uint64_t)a * (uint64_t)b);
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
        neon_trap("integer overflow");
    }
    return a % b;
}

int64_t neon_i64_neg(int64_t a) {
    return (int64_t)(-(uint64_t)a);
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

// Byte-lexicographic order: the shared prefix decides, and if one string is a prefix of
// the other the shorter sorts first. `memcmp`'s sign is only guaranteed meaningful over
// the common length, hence comparing lengths separately rather than over the longer one.
// This is bytes, not codepoints and not collation -- `byte_len`'s naming rule applies.
int neon_str_cmp(neon_str a, neon_str b) {
    size_t n = a.len < b.len ? a.len : b.len;
    int c = n ? memcmp(a.data, b.data, n) : 0;
    if (c != 0) {
        return c < 0 ? -1 : 1;
    }
    return a.len < b.len ? -1 : (a.len > b.len ? 1 : 0);
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

// The `+` operator. It borrows both operands -- the IR treats a `prim.add`'s inputs as
// borrowed and releases them itself at their last use -- so this must not release them.
neon_str neon_str_add(neon_str a, neon_str b) {
    neon_header* h = neon_alloc(a.len + b.len, neon_str_drop);
    char* data = (char*)(h + 1);
    memcpy(data, a.data, a.len);
    memcpy(data + a.len, b.data, b.len);
    neon_str s = {data, a.len + b.len, h};
    return s;
}

// ---- string natives (consume their str arguments) ----

// The byte offset of `needle` in `hay`, or -1. An empty needle is found at 0.
static int64_t str_index_of(neon_str hay, neon_str needle) {
    if (needle.len == 0) return 0;
    if (needle.len > hay.len) return -1;
    for (size_t i = 0; i + needle.len <= hay.len; i++) {
        if (memcmp(hay.data + i, needle.data, needle.len) == 0) return (int64_t)i;
    }
    return -1;
}

int64_t neon_str_byte_len(neon_str s) {
    int64_t r = (int64_t)s.len;
    neon_release(s.owner);
    return r;
}

bool neon_str_is_empty(neon_str s) {
    bool r = s.len == 0;
    neon_release(s.owner);
    return r;
}

neon_str neon_str_to_upper(neon_str s) {
    neon_str r = neon_str_new(s.data, s.len);
    for (size_t i = 0; i < r.len; i++) {
        char c = r.data[i];
        if (c >= 'a' && c <= 'z') r.data[i] = (char)(c - 32);
    }
    neon_release(s.owner);
    return r;
}

neon_str neon_str_to_lower(neon_str s) {
    neon_str r = neon_str_new(s.data, s.len);
    for (size_t i = 0; i < r.len; i++) {
        char c = r.data[i];
        if (c >= 'A' && c <= 'Z') r.data[i] = (char)(c + 32);
    }
    neon_release(s.owner);
    return r;
}

neon_str neon_str_repeat(neon_str s, int64_t n) {
    if (n <= 0) {
        neon_release(s.owner);
        return neon_str_lit("", 0);
    }
    size_t total = s.len * (size_t)n;
    neon_header* h = neon_alloc(total, neon_str_drop);
    char* data = (char*)(h + 1);
    for (int64_t i = 0; i < n; i++) memcpy(data + (size_t)i * s.len, s.data, s.len);
    neon_str r = {data, total, h};
    neon_release(s.owner);
    return r;
}

bool neon_str_contains(neon_str s, neon_str needle) {
    bool r = str_index_of(s, needle) >= 0;
    neon_release(s.owner);
    neon_release(needle.owner);
    return r;
}

bool neon_str_starts_with(neon_str s, neon_str prefix) {
    bool r = prefix.len <= s.len && memcmp(s.data, prefix.data, prefix.len) == 0;
    neon_release(s.owner);
    neon_release(prefix.owner);
    return r;
}

bool neon_str_ends_with(neon_str s, neon_str suffix) {
    bool r = suffix.len <= s.len &&
             memcmp(s.data + s.len - suffix.len, suffix.data, suffix.len) == 0;
    neon_release(s.owner);
    neon_release(suffix.owner);
    return r;
}

// A byte slice: `str` is byte-indexed throughout (`byte_len`, `find`), so this cuts at
// byte offsets and may split a UTF-8 sequence — the caller asked for bytes.
neon_str neon_str_slice_unchecked(neon_str s, int64_t from, int64_t to) {
    neon_str r = neon_str_new(s.data + from, (size_t)(to - from));
    neon_release(s.owner);
    return r;
}

// The single byte at `i`. `str` is byte-indexed throughout, so this indexes bytes and may
// land inside a UTF-8 sequence — the same contract as `slice` and `find`.
neon_str neon_str_char_at_unchecked(neon_str s, int64_t i) {
    neon_str r = neon_str_new(s.data + i, 1);
    neon_release(s.owner);
    return r;
}

int64_t neon_str_index_of(neon_str s, neon_str needle) {
    int64_t r = str_index_of(s, needle);
    neon_release(s.owner);
    neon_release(needle.owner);
    return r;
}

// Whether the whole string is a decimal integer, optionally signed. Kept separate from
// parsing so the Neon wrapper decides what to throw.
bool neon_str_is_int(neon_str s) {
    size_t i = 0;
    if (s.len > 0 && (s.data[0] == '-' || s.data[0] == '+')) i = 1;
    bool any = false;
    for (; i < s.len; i++) {
        if (s.data[i] < '0' || s.data[i] > '9') {
            neon_release(s.owner);
            return false;
        }
        any = true;
    }
    neon_release(s.owner);
    return any;
}

int64_t neon_str_parse_int(neon_str s) {
    int64_t sign = 1, v = 0;
    size_t i = 0;
    if (s.len > 0 && (s.data[0] == '-' || s.data[0] == '+')) {
        sign = s.data[0] == '-' ? -1 : 1;
        i = 1;
    }
    for (; i < s.len; i++) {
        v = (int64_t)((uint64_t)v * 10 + (uint64_t)(s.data[i] - '0'));
    }
    neon_release(s.owner);
    return (int64_t)((uint64_t)v * (uint64_t)sign);
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

// A NUL-terminated copy of a `neon_str`, for the C APIs that demand one. `neon_str` is a
// length-delimited *view* -- a slice of a larger buffer is not terminated -- so this cannot
// be skipped by passing `.data`. Caller frees.
static char* neon_cstr(neon_str s) {
    char* p = (char*)malloc(s.len + 1);
    if (p == NULL) neon_trap("out of memory");
    if (s.len) memcpy(p, s.data, s.len);
    p[s.len] = 0;
    return p;
}
// ---- resources ----
//
// The shared tail of every instantiation's drop. The instantiation's own drop runs
// cleanup -- it is the only code that knows the payload's type and how to call the
// closure -- and then lands here.
//
// Cleanup's failure is discarded on this path: a drop has no error channel, which is
// exactly why an explicit `release` exists.
//
// Resurrection is unreachable by construction: cleanup receives the *payload*, never the
// resource, so it has nothing to store, and captures are by value and sealed. The
// assertion is cheap -- it sits on a path that has just made a syscall -- and would fire
// loudly if the language ever grew mutable shared state.
void neon_resource_finish(neon_resource* r) {
    if (r->w && r->w->release) {
        r->w->release(neon_resource_payload(r));
    }
    if (r->cleanup.env) {
        neon_release(r->cleanup.env);
    }
    assert(r->header.rc == 0 && "a resource was resurrected during cleanup");
    neon_free(r);
}

neon_resource* neon_resource_new(const void* payload, const neon_witness* w,
                                 neon_closure cleanup, void (*drop)(void*)) {
    size_t extra = sizeof(neon_resource) - sizeof(neon_header) + w->size;
    neon_resource* r = (neon_resource*)neon_alloc(extra, drop);
    r->w = w;
    r->cleanup = cleanup;
    r->armed = true;
    memcpy(neon_resource_payload(r), payload, w->size);
    return r;
}

// These consume `r`, like every other native taking a counted pointer: the caller's
// reference moves in, so each releases it before returning. Releasing may be the last
// reference, in which case the drop runs cleanup right here -- which is what last-use ARC
// means, and why the payload is retained before that can happen.
bool neon_resource_get(neon_resource* r, void* out) {
    bool live = r->armed;
    if (live) {
        memcpy(out, neon_resource_payload(r), r->w->size);
        // The caller receives an owned value, like every other reader in this ABI.
        if (r->w->retain) {
            r->w->retain(out);
        }
    }
    neon_release((neon_header*)r);
    return live;
}

bool neon_resource_disarm(neon_resource* r, void* out) {
    bool armed = r->armed;
    if (armed) {
        // Disarm *first*: whoever gets `true` owns the cleanup, and there is exactly one
        // of them. The payload moves out, so the drop must not release it again.
        r->armed = false;
        memcpy(out, neon_resource_payload(r), r->w->size);
        memset(neon_resource_payload(r), 0, r->w->size);
    }
    neon_release((neon_header*)r);
    return armed;
}

// Hands back an owned closure, so its environment is retained before `r` goes: releasing
// `r` may be the last reference, and the environment would die with it.
neon_closure neon_resource_cleanup(neon_resource* r) {
    neon_closure c = r->cleanup;
    if (c.env) {
        neon_retain(c.env);
    }
    neon_release((neon_header*)r);
    return c;
}

bool neon_resource_is_live(neon_resource* r) {
    bool live = r->armed;
    neon_release((neon_header*)r);
    return live;
}


// ---- files ----
//
// Descriptors, not `FILE*`: `writev` wants one, and buffering here would only sit between
// the caller's iolist and the kernel.
//
// Failure travels as a *value*. Every fallible call returns `-errno`, and the ones that
// also return data use an out-parameter, which codegen turns into a Neon tuple (see
// `emit_native_out`). An earlier draft kept an errno-style flag in a static; any
// intervening call could clobber it and it said nothing at the call site.
//
// `neon_io_strerror` is a pure function of a code, so rendering a failure needs no state
// at all.

// `mode`: 0 read, 1 write (truncate), 2 append. The flags stay on this side because they
// are platform constants.
int64_t neon_io_open(neon_str path, int64_t mode) {
    char* p = neon_cstr(path);
    int flags = mode == 0   ? O_RDONLY
                : mode == 1 ? (O_WRONLY | O_CREAT | O_TRUNC)
                            : (O_WRONLY | O_CREAT | O_APPEND);
    int fd = open(p, flags, 0666);
    int64_t r = fd < 0 ? -(int64_t)errno : (int64_t)fd;
    free(p);
    neon_release(path.owner);
    return r;
}

// A bare descriptor: the armed flag that stops a double close now lives in the
// `Resource` wrapping this, not here.
int64_t neon_io_close(int64_t fd) {
    return close((int)fd) != 0 ? -(int64_t)errno : 0;
}

neon_str neon_io_strerror(int64_t code) {
    const char* m = strerror(code < 0 ? (int)-code : (int)code);
    return neon_str_new(m, strlen(m));
}

// Everything left in the descriptor. `err` is the out-parameter: 0, or `-errno`. The data
// and the status come back together rather than through hidden state.
neon_str neon_io_read_all(int64_t fd, int64_t* err) {
    *err = 0;
    size_t cap = 4096, len = 0;
    char* buf = (char*)malloc(cap);
    if (buf == NULL) neon_trap("out of memory");
    for (;;) {
        if (len == cap) {
            cap *= 2;
            char* grown = (char*)realloc(buf, cap);
            if (grown == NULL) neon_trap("out of memory");
            buf = grown;
        }
        ssize_t got = read((int)fd, buf + len, cap - len);
        if (got < 0) {
            if (errno == EINTR) continue;
            *err = -(int64_t)errno;
            break;
        }
        if (got == 0) break;
        len += (size_t)got;
    }
    neon_str r = neon_str_new(buf, len);
    free(buf);
    return r;
}

// Write a list of `neon_str` views as one `writev`, so the pieces reach the kernel without
// ever being concatenated. `neon_str` is `{data, len, owner}` and `iovec` is
// `{iov_base, iov_len}` -- the first two fields line up, but the strides differ, so this
// copies pointer/length pairs and never a payload byte.
//
// Longer lists go in batches, and a short write resumes where it stopped rather than
// counting as failure -- that is the contract `writev` actually offers.
int64_t neon_io_writev(int64_t fd, neon_list* parts) {
    const neon_str* items = (const neon_str*)parts->data;
    int64_t status = 0;
    size_t i = 0;
    while (status == 0 && i < parts->len) {
        size_t batch = parts->len - i;
        if (batch > NEON_IOV_MAX) batch = NEON_IOV_MAX;
        struct iovec vec[NEON_IOV_MAX];
        size_t n = 0, total = 0;
        for (size_t j = 0; j < batch; j++) {
            if (items[i + j].len == 0) continue; // an empty piece is not a write
            vec[n].iov_base = items[i + j].data;
            vec[n].iov_len = items[i + j].len;
            total += items[i + j].len;
            n++;
        }
        i += batch;
        size_t done = 0, first = 0;
        while (done < total) {
            ssize_t got = writev((int)fd, vec + first, (int)(n - first));
            if (got < 0) {
                if (errno == EINTR) continue;
                status = -(int64_t)errno;
                break;
            }
            done += (size_t)got;
            size_t adv = (size_t)got;
            while (first < n && adv >= vec[first].iov_len) {
                adv -= vec[first].iov_len;
                first++;
            }
            if (first < n && adv) {
                vec[first].iov_base = (char*)vec[first].iov_base + adv;
                vec[first].iov_len -= adv;
            }
        }
    }
    neon_release((neon_header*)parts);
    return status;
}

int64_t neon_io_remove(neon_str path) {
    char* p = neon_cstr(path);
    int64_t r = remove(p) == 0 ? 0 : -(int64_t)errno;
    free(p);
    neon_release(path.owner);
    return r;
}

bool neon_io_exists(neon_str path) {
    char* p = neon_cstr(path);
    bool ok = access(p, F_OK) == 0;
    free(p);
    neon_release(path.owner);
    return ok;
}

// ---- math ----
//
// Thin over libm. `f64` keeps IEEE semantics throughout the language (see "Comparison is
// structural" in docs/decisions.md), so these do not trap or throw: `sqrt(-1)` is NaN,
// `1.0/0.0` is infinity, and a caller who cares tests for them. That is consistent with
// `==` and `<`, which already answer the IEEE way for NaN.
//
// `i64` is the opposite and stays so: `neon_i64_abs(INT64_MIN)` has no representable
// answer, so it traps, exactly as division does.
double neon_f64_sqrt(double x) { return sqrt(x); }
double neon_f64_pow(double a, double b) { return pow(a, b); }
double neon_f64_floor(double x) { return floor(x); }
double neon_f64_ceil(double x) { return ceil(x); }
double neon_f64_round(double x) { return round(x); }
double neon_f64_abs(double x) { return fabs(x); }
bool neon_f64_is_nan(double x) { return x != x; }
bool neon_f64_is_infinite(double x) { return isinf(x) != 0; }

int64_t neon_i64_abs(int64_t a) {
    if (a == INT64_MIN) {
        neon_trap("integer overflow");
    }
    return a < 0 ? -a : a;
}

// `f64` from `i64` is exact only up to 2^53; beyond that it rounds, like every language
// with these two types. Truncation toward zero the other way, and a value outside the
// integer range traps rather than being undefined -- a C cast there is UB.
double neon_i64_to_f64(int64_t a) { return (double)a; }

int64_t neon_f64_to_i64(double x) {
    if (x != x || x >= 9223372036854775808.0 || x < -9223372036854775808.0) {
        neon_trap("f64 out of i64 range");
    }
    return (int64_t)x;
}

// Fixed-point rendering, for `fmt`. `%g` (what `to_string` uses) is right for "show me
// this number" and wrong for a table, which needs a fixed width.
neon_str neon_f64_to_fixed(double x, int64_t places) {
    if (places < 0) places = 0;
    if (places > 17) places = 17;
    char buf[64];
    int len = snprintf(buf, sizeof buf, "%.*f", (int)places, x);
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

// ---- list ----

int64_t neon_list_len(neon_list* l) {
    int64_t n = (int64_t)l->len;
    neon_release((neon_header*)l);
    return n;
}

void* neon_list_at(neon_list* l, int64_t i) {
    if (i < 0 || (size_t)i >= l->len) {
        neon_trap("list index out of range");
    }
    return l->data + (size_t)i * l->w->size;
}

static void neon_list_drop(void* p) {
    neon_list* l = (neon_list*)p;
    if (l->w->release) {
        for (size_t i = 0; i < l->len; i++) {
            l->w->release(l->data + i * l->w->size);
        }
    }
    free(l->data);
    neon_free(l); // frees the header+body allocation
}

neon_list* neon_list_new(const neon_witness* w) {
    neon_list* l = (neon_list*)neon_alloc(sizeof(neon_list) - sizeof(neon_header), neon_list_drop);
    l->w = w;
    l->len = 0;
    l->cap = 0;
    l->data = NULL;
    return l;
}

neon_list* neon_list_new_with_capacity(const neon_witness* w, int64_t cap) {
    neon_list* l = neon_list_new(w);
    if (cap > 0) {
        l->cap = (size_t)cap;
        l->data = (char*)malloc((size_t)cap * w->size);
        if (l->data == NULL) neon_trap("out of memory");
    }
    return l;
}


// Copy a shared list before a mutation, retaining each element for the copy.
static neon_list* neon_list_ensure_unique(neon_list* l) {
    if (l->header.rc == 1) {
        return l;
    }
    size_t sz = l->w->size;
    neon_list* c = neon_list_new_with_capacity(l->w, (int64_t)(l->len ? l->len : 1));
    memcpy(c->data, l->data, l->len * sz);
    c->len = l->len;
    if (l->w->retain) {
        for (size_t i = 0; i < c->len; i++) l->w->retain(c->data + i * sz);
    }
    neon_release((neon_header*)l);
    return c;
}


neon_list* neon_list_push(neon_list* l, const void* elem) {
    size_t sz = l->w->size;
    l = neon_list_ensure_unique(l);
    if (l->len == l->cap) {
        size_t ncap = l->cap ? l->cap * 2 : 4;
        l->data = (char*)realloc(l->data, ncap * sz);
        if (l->data == NULL) neon_trap("out of memory");
        l->cap = ncap;
    }
    memcpy(l->data + l->len * sz, elem, sz);
    l->len++;
    return l;
}

neon_list* neon_list_set(neon_list* l, int64_t i, const void* elem) {
    if (i < 0 || (size_t)i >= l->len) {
        neon_trap("list index out of range");
    }
    size_t sz = l->w->size;
    l = neon_list_ensure_unique(l);
    char* slot = l->data + (size_t)i * sz;
    if (l->w->release) l->w->release(slot);
    memcpy(slot, elem, sz);
    return l;
}

neon_list* neon_list_concat(neon_list* a, neon_list* b) {
    size_t sz = a->w->size;
    neon_list* r = neon_list_new_with_capacity(a->w, (int64_t)(a->len + b->len));
    memcpy(r->data, a->data, a->len * sz);
    memcpy(r->data + a->len * sz, b->data, b->len * sz);
    r->len = a->len + b->len;
    if (a->w->retain) {
        for (size_t i = 0; i < r->len; i++) a->w->retain(r->data + i * sz);
    }
    neon_release((neon_header*)a);
    neon_release((neon_header*)b);
    return r;
}

// Lexicographic over elements: the first differing element decides, and if one list is a
// prefix of the other the shorter sorts first. Both lists are borrowed -- comparison reads.
//
// The element compare comes from the witness, so this one function serves every element
// type, including nested lists: an inner list's elements are reached through *its* witness
// on the recursive call.
int neon_list_cmp(const neon_list* a, const neon_list* b) {
    size_t sz = a->w->size;
    size_t n = a->len < b->len ? a->len : b->len;
    for (size_t i = 0; i < n; i++) {
        int c = a->w->cmp(a->data + i * sz, b->data + i * sz);
        if (c != 0) {
            return c;
        }
    }
    return a->len < b->len ? -1 : (a->len > b->len ? 1 : 0);
}

// Equality could be `neon_list_cmp(a, b) == 0`, but a length check rejects most unequal
// pairs without touching an element, and it is the answer `==` asks for.
bool neon_list_eq(const neon_list* a, const neon_list* b) {
    if (a->len != b->len) {
        return false;
    }
    size_t sz = a->w->size;
    for (size_t i = 0; i < a->len; i++) {
        if (!a->w->eq(a->data + i * sz, b->data + i * sz)) {
            return false;
        }
    }
    return true;
}

neon_str neon_str_join(neon_list* parts, neon_str sep) {
    size_t total = 0;
    for (size_t i = 0; i < parts->len; i++) {
        total += ((neon_str*)parts->data)[i].len;
    }
    if (parts->len > 1) total += sep.len * (parts->len - 1);

    neon_header* h = neon_alloc(total, neon_str_drop);
    char* data = (char*)(h + 1);
    size_t off = 0;
    for (size_t i = 0; i < parts->len; i++) {
        if (i > 0) {
            memcpy(data + off, sep.data, sep.len);
            off += sep.len;
        }
        neon_str e = ((neon_str*)parts->data)[i];
        memcpy(data + off, e.data, e.len);
        off += e.len;
    }
    neon_str s = {data, total, h};
    neon_release((neon_header*)parts); // consumes parts (drops its str elements)
    neon_release(sep.owner);
    return s;
}

// ---- any (boxed erasure) ----

static void neon_box_drop(void* p) {
    neon_box* b = (neon_box*)p;
    if (b->w->release) {
        b->w->release((void*)(b + 1));
    }
    neon_free(b);
}

neon_value neon_box_new(const void* payload, const neon_witness* w, uint64_t tag) {
    size_t extra = sizeof(neon_box) - sizeof(neon_header) + w->size;
    neon_box* b = (neon_box*)neon_alloc(extra, neon_box_drop);
    b->w = w;
    b->type_tag = tag;
    memcpy((void*)(b + 1), payload, w->size);
    return (neon_value)b;
}

// ---- map ----
//
// Open addressing with linear probing. `ctrl` marks each slot empty, dead (a tombstone
// left by a removal) or full; keys and values sit in parallel arrays sized by their
// witnesses. Equality and hashing come from the key witness, so a `str` key compares by
// content rather than by address.

static void neon_map_drop(void* p) {
    neon_map* m = (neon_map*)p;
    for (size_t i = 0; i < m->cap; i++) {
        if (m->ctrl[i] != NEON_MAP_FULL) continue;
        if (m->kw->value->release) m->kw->value->release(m->keys + i * m->kw->value->size);
        if (m->vw->release) m->vw->release(m->vals + i * m->vw->size);
    }
    free(m->ctrl);
    free(m->keys);
    free(m->vals);
    neon_free(m);
}

static neon_map* neon_map_alloc(const neon_key_witness* kw, const neon_witness* vw, size_t cap) {
    neon_map* m = (neon_map*)neon_alloc(sizeof(neon_map) - sizeof(neon_header), neon_map_drop);
    m->kw = kw;
    m->vw = vw;
    m->len = 0;
    m->cap = cap;
    m->ctrl = (unsigned char*)calloc(cap ? cap : 1, 1);
    m->keys = (char*)malloc((cap ? cap : 1) * kw->value->size);
    m->vals = (char*)malloc((cap ? cap : 1) * vw->size);
    if (m->ctrl == NULL || m->keys == NULL || m->vals == NULL) neon_trap("out of memory");
    return m;
}

neon_map* neon_map_new(const neon_key_witness* kw, const neon_witness* vw) {
    return neon_map_alloc(kw, vw, 8);
}

// The slot a key belongs in: its own if present, else the first free slot on its probe.
static size_t neon_map_slot(neon_map* m, const void* key, bool* found) {
    size_t ksz = m->kw->value->size;
    size_t mask = m->cap - 1;
    size_t i = (size_t)m->kw->hash(key) & mask;
    size_t first_dead = (size_t)-1;
    for (size_t n = 0; n < m->cap; n++) {
        unsigned char c = m->ctrl[i];
        if (c == NEON_MAP_EMPTY) {
            *found = false;
            return first_dead != (size_t)-1 ? first_dead : i;
        }
        if (c == NEON_MAP_DEAD) {
            if (first_dead == (size_t)-1) first_dead = i;
        } else if (m->kw->eq(m->keys + i * ksz, key)) {
            *found = true;
            return i;
        }
        i = (i + 1) & mask;
    }
    *found = false;
    return first_dead != (size_t)-1 ? first_dead : 0;
}

// Release a key the map is not going to store. Every map native *consumes* its key, the
// same convention every other native follows: the caller hands over one reference and does
// not release it afterwards. `set` discharges that by moving the key into the table, and
// the lookups discharge it here. Missing this leaked a key per lookup -- invisible for an
// `i64` or a literal `str`, and 72 bytes a call for a `List` key.
static void neon_map_release_key(neon_map* m, const void* key) {
    if (m->kw->value->release) {
        m->kw->value->release((void*)key);
    }
}

void* neon_map_find(neon_map* m, const void* key) {
    bool found = false;
    size_t i = neon_map_slot(m, key, &found);
    return found ? m->vals + i * m->vw->size : NULL;
}

// Borrows its key, unlike `contains` and `set`. It is reached through `Op::Index` rather
// than `Op::Native`, and the refcount pass releases an index's operands itself -- releasing
// here as well double-freed the key.
void* neon_map_at(neon_map* m, const void* key) {
    void* v = neon_map_find(m, key);
    if (v == NULL) {
        neon_trap("key not present");
    }
    return v;
}

int64_t neon_map_len(neon_map* m) {
    int64_t n = (int64_t)m->len;
    neon_release((neon_header*)m);
    return n;
}

bool neon_map_contains(neon_map* m, const void* key) {
    bool r = neon_map_find(m, key) != NULL;
    neon_map_release_key(m, key);
    neon_release((neon_header*)m);
    return r;
}

// A fresh map holding everything this one does, retaining each entry it copies.
static neon_map* neon_map_clone(neon_map* m, size_t cap) {
    neon_map* c = neon_map_alloc(m->kw, m->vw, cap);
    size_t ksz = m->kw->value->size, vsz = m->vw->size;
    for (size_t i = 0; i < m->cap; i++) {
        if (m->ctrl[i] != NEON_MAP_FULL) continue;
        bool found = false;
        size_t j = neon_map_slot(c, m->keys + i * ksz, &found);
        memcpy(c->keys + j * ksz, m->keys + i * ksz, ksz);
        memcpy(c->vals + j * vsz, m->vals + i * vsz, vsz);
        c->ctrl[j] = NEON_MAP_FULL;
        c->len++;
        if (m->kw->value->retain) m->kw->value->retain(c->keys + j * ksz);
        if (m->vw->retain) m->vw->retain(c->vals + j * vsz);
    }
    return c;
}

// Drop `key` if present. Consumes the map and the key, like `set`.
//
// The slot becomes a tombstone rather than empty: a probe that walked past this slot to
// place a later key must still walk past it, and marking it empty would cut that chain and
// lose the entry. `len` drops, so the load factor still falls.
neon_map* neon_map_remove(neon_map* m, const void* key) {
    // Copy before mutating when shared, exactly as `set` does -- removal is a mutation, and
    // these are values.
    if (m->header.rc > 1) {
        neon_map* c = neon_map_clone(m, m->cap);
        neon_release((neon_header*)m);
        m = c;
    }
    bool found = false;
    size_t i = neon_map_slot(m, key, &found);
    if (found) {
        size_t ksz = m->kw->value->size, vsz = m->vw->size;
        if (m->kw->value->release) m->kw->value->release(m->keys + i * ksz);
        if (m->vw->release) m->vw->release(m->vals + i * vsz);
        m->ctrl[i] = NEON_MAP_DEAD;
        m->len--;
    }
    neon_map_release_key(m, key);
    return m;
}

// Two maps are equal when they hold the same set of keys with equal values. Borrows both.
//
// Iteration order is not part of the answer, so this looks each key up in `b` rather than
// walking the two slot arrays in step: the same entries can sit at different slots after a
// different insertion history, and an open-addressed table has no canonical order.
// Comparing lengths first makes "same keys" enough -- if every key of `a` is in `b` and the
// counts match, neither can hold a key the other lacks.
bool neon_map_eq(neon_map* a, neon_map* b) {
    if (a == b) {
        return true;
    }
    if (a->len != b->len) {
        return false;
    }
    size_t ksz = a->kw->value->size, vsz = a->vw->size;
    for (size_t i = 0; i < a->cap; i++) {
        if (a->ctrl[i] != NEON_MAP_FULL) {
            continue;
        }
        const void* key = a->keys + i * ksz;
        void* other = neon_map_find(b, key);
        if (other == NULL || !a->vw->eq(a->vals + i * vsz, other)) {
            return false;
        }
    }
    return true;
}

neon_map* neon_map_set(neon_map* m, const void* key, const void* val) {
    // Shared, or too full to probe well: copy before mutating. Uniquely owned maps are
    // updated in place, which is what makes the immutable interface cheap.
    if (m->header.rc > 1 || (m->len + 1) * 4 >= m->cap * 3) {
        size_t cap = (m->len + 1) * 4 >= m->cap * 3 ? m->cap * 2 : m->cap;
        neon_map* c = neon_map_clone(m, cap);
        neon_release((neon_header*)m);
        m = c;
    }
    size_t ksz = m->kw->value->size, vsz = m->vw->size;
    bool found = false;
    size_t i = neon_map_slot(m, key, &found);
    if (found) {
        // The table keeps the key it already has, so the incoming one is ours to drop.
        neon_map_release_key(m, key);
        if (m->vw->release) m->vw->release(m->vals + i * vsz);
    } else {
        memcpy(m->keys + i * ksz, key, ksz);
        m->ctrl[i] = NEON_MAP_FULL;
        m->len++;
    }
    memcpy(m->vals + i * vsz, val, vsz);
    return m;
}

// `keys`/`values` hand back a list; the element witness comes from codegen, which knows
// the concrete element type.
static neon_list* neon_map_collect(neon_map* m, const neon_witness* w, bool want_keys) {
    neon_list* out = neon_list_new_with_capacity(w, (int64_t)m->len);
    size_t esz = w->size;
    size_t ksz = m->kw->value->size, vsz = m->vw->size;
    for (size_t i = 0; i < m->cap; i++) {
        if (m->ctrl[i] != NEON_MAP_FULL) continue;
        const char* src = want_keys ? m->keys + i * ksz : m->vals + i * vsz;
        memcpy(out->data + out->len * esz, src, esz);
        if (w->retain) w->retain(out->data + out->len * esz);
        out->len++;
    }
    neon_release((neon_header*)m);
    return out;
}

neon_list* neon_map_keys(neon_map* m, const neon_witness* w) {
    return neon_map_collect(m, w, true);
}

neon_list* neon_map_values(neon_map* m, const neon_witness* w) {
    return neon_map_collect(m, w, false);
}
