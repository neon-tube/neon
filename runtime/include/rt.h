#ifndef NEON_RT_H
#define NEON_RT_H

// The Neon runtime: the ABI the emitted C shares with hand-written natives. See
// docs/design/ir.md. This is the minimal core -- header + refcount, str, trapping i64
// arithmetic, and the first natives -- growing with the backend.

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>

// Every heap object begins with this. `drop` frees *this* object (releasing its own
// counted fields first); a NULL owner / an immortal flag makes retain/release no-ops.
typedef struct neon_header {
    uint64_t rc;
    uint32_t flags;
    void (*drop)(void*);
} neon_header;

#define NEON_IMMORTAL 1u

// A string is a view: a data pointer and length (the pair libc wants), plus the
// refcounted allocation it points into. A literal has owner == NULL: static, never freed.
typedef struct {
    char* data;
    size_t len;
    neon_header* owner;
} neon_str;

typedef struct { char _unit; } neon_unit;
typedef struct { void* fn; neon_header* env; } neon_closure;
typedef void* neon_value;

// A generic container carries the value-witness for its element type: its size, and how to
// retain/release one in place (NULL when the element holds nothing counted, e.g. a scalar).
// Only bulk operations (grow, clone, drop-all) use it; element access is emitted inline by
// codegen, which knows the type statically.
// `eq` and `cmp` compare two elements structurally: `eq` for `==`, `cmp` three-way for
// `<` (the `memcmp` convention). They live here rather than in a layered witness of their
// own -- the way hashing does -- because the argument that kept `hash` out does not apply:
// only a *map key* is hashed, so most element types would carry a hash pointer forever,
// whereas any list can be compared and so any element type may need these.
//
// `eq` is always present: equality is total on every type. `cmp` is NULL when the element
// has no structural order (a union -- ordering one would need an invented rank between its
// arms); the checker rejects ordering such a list, so a non-NULL `cmp` is the caller's
// precondition, not something to test at run time.
typedef struct neon_witness {
    size_t size;
    void (*retain)(void* elem);
    void (*release)(void* elem);
    bool (*eq)(const void* a, const void* b);
    int (*cmp)(const void* a, const void* b);
} neon_witness;

// What a *hashed* container additionally needs of its key type. Layered rather than folded
// into `neon_witness`, which every container element has one of: only map keys are hashed,
// so most types would carry two null pointers forever. Equality is by content — a `str`
// key compares its bytes, not its address — so both are emitted per type by codegen.
typedef struct neon_key_witness {
    const neon_witness* value;
    uint64_t (*hash)(const void* key);
    bool (*eq)(const void* a, const void* b);
} neon_key_witness;

// FNV-1a, the hash the atom tags and boxed type tags already use.
static inline uint64_t neon_hash_bytes(const void* p, size_t n) {
    const unsigned char* b = (const unsigned char*)p;
    uint64_t h = 0xcbf29ce484222325ULL;
    for (size_t i = 0; i < n; i++) {
        h ^= b[i];
        h *= 0x100000001b3ULL;
    }
    return h;
}
static inline uint64_t neon_hash_mix(uint64_t a, uint64_t b) {
    return (a ^ b) * 0x100000001b3ULL;
}

// A list stores its elements inline in `data` (len used of cap slots, each `w->size`
// bytes). The header is first, so a `neon_list*` is also its `neon_header*`.
typedef struct neon_list {
    neon_header header;
    const neon_witness* w;
    size_t len;
    size_t cap;
    char* data;
} neon_list;

// An open-addressed hash map. `ctrl` marks each slot empty/tombstone/full, and keys and
// values live in parallel arrays sized by their witnesses. The header is first, so a
// `neon_map*` is also its `neon_header*`.
#define NEON_MAP_EMPTY 0u
#define NEON_MAP_DEAD 1u
#define NEON_MAP_FULL 2u

typedef struct neon_map {
    neon_header header;
    const neon_key_witness* kw;
    const neon_witness* vw;
    size_t len;
    size_t cap;
    unsigned char* ctrl;
    char* keys;
    char* vals;
} neon_map;

// ---- lifecycle ----
void neon_rt_init(void);
void neon_retain(neon_header* h);
void neon_release(neon_header* h);
void* neon_alloc(size_t bytes, void (*drop)(void*));
void neon_free(void* p);

// ---- traps (print + _exit; no unwind, no teardown) ----
_Noreturn void neon_trap(const char* msg);
_Noreturn void neon_panic(neon_str msg);
_Noreturn void neon_unreachable(void);

// ---- scalar helpers ----
static inline neon_unit neon_unit_v(void) {
    neon_unit u = {0};
    return u;
}
static inline neon_unit neon_null(void) {
    neon_unit u = {0};
    return u;
}
static inline uint64_t neon_atom(uint64_t hash) {
    return hash;
}
static inline double neon_f64_bits(uint64_t bits) {
    double d;
    memcpy(&d, &bits, sizeof d);
    return d;
}

// ---- trapping i64 arithmetic ----
int64_t neon_i64_add(int64_t a, int64_t b);
int64_t neon_i64_sub(int64_t a, int64_t b);
int64_t neon_i64_mul(int64_t a, int64_t b);
int64_t neon_i64_div(int64_t a, int64_t b);
int64_t neon_i64_rem(int64_t a, int64_t b);
int64_t neon_i64_neg(int64_t a);

// ---- files ----
//
// Descriptors, and only descriptors. The *handle* is `opaque record File` on the Neon
// side, holding a `Resource[i64, IoError]` -- so refcounted cleanup, the armed flag and
// use-after-close detection all come from `neon_resource` instead of being open-coded
// here. This file used to carry a `neon_file` doing exactly that by hand.
//
// Failure is a value (`-errno`); the one call that returns data as well uses an
// out-parameter, which codegen turns into a tuple.

// ---- resources: a payload, a cleanup, and an armed flag ----
//
// The generalisation of what `neon_file` does by hand. A resource owns a payload and a
// cleanup that runs exactly once: either explicitly, so the caller sees the error, or on
// the last release, where there is no error channel and the failure is discarded. The
// armed flag is what makes those two paths safe together -- disarm-then-act, so a double
// release is a no-op rather than a second `close` landing on a reused descriptor.
//
// The payload is stored inline after the struct, sized by its witness, so a resource is
// one allocation regardless of what it holds.
//
// `cleanup` is the user's closure, which varies per call; the code that knows how to
// *call* it varies per instantiation and is emitted by codegen. That typed code is
// reached through `header.drop`, not through a field of its own: `neon_alloc` already
// takes a per-object drop, and one indirection is enough. `neon_file_drop` is the same
// shape by hand.
//
// Only the drop path needs it. `release` is an ordinary Neon function that disarms, takes
// the payload and calls the closure itself, so the explicit path is fully typed and its
// error propagates normally -- which is the whole reason that path exists.
typedef struct neon_resource {
    neon_header header;
    const neon_witness* w;
    neon_closure cleanup;
    bool armed;
} neon_resource;

static inline void* neon_resource_payload(neon_resource* r) {
    return (void*)(r + 1);
}

// `drop` is the instantiation's own drop, emitted by codegen: it runs cleanup with a
// typed payload if still armed, then calls `neon_resource_finish`.
neon_resource* neon_resource_new(const void* payload, const neon_witness* w,
                                 neon_closure cleanup, void (*drop)(void*));
// The shared tail of every instantiation's drop: release the payload's counted parts and
// the closure's environment, then free. Split out so the emitted drop is a few lines.
void neon_resource_finish(neon_resource* r);
// The cleanup closure, for the explicit `release` path to call from Neon.
neon_closure neon_resource_cleanup(neon_resource* r);
// Read the payload without consuming the resource. `false` when already released, in
// which case `out` is untouched -- this is what turns use-after-release into a
// diagnosable error rather than a read of a stale descriptor.
bool neon_resource_get(neon_resource* r, void* out);
// Disarm and hand back the payload for the caller to clean up, `false` if already
// disarmed. Disarming *first* is the whole safety property: whoever gets `true` owns the
// cleanup, and there is exactly one of them.
bool neon_resource_disarm(neon_resource* r, void* out);
bool neon_resource_is_live(neon_resource* r);

int64_t neon_io_open(neon_str path, int64_t mode);      // consumes path; fd or -errno
int64_t neon_io_close(int64_t fd);                      // 0 or -errno
neon_str neon_io_read_all(int64_t fd, int64_t* err);    // *err: 0 or -errno
int64_t neon_io_writev(int64_t fd, neon_list* parts);   // consumes parts; 0 or -errno
int64_t neon_io_remove(neon_str path);                  // consumes path; 0 or -errno
bool neon_io_exists(neon_str path);                     // consumes path
neon_str neon_io_strerror(int64_t code);                // pure: a code, not hidden state

// ---- math (IEEE for f64: no traps, NaN and infinity propagate; i64 traps) ----
double neon_f64_sqrt(double x);
double neon_f64_pow(double a, double b);
double neon_f64_floor(double x);
double neon_f64_ceil(double x);
double neon_f64_round(double x);
double neon_f64_abs(double x);
bool neon_f64_is_nan(double x);
bool neon_f64_is_infinite(double x);
int64_t neon_i64_abs(int64_t a);
double neon_i64_to_f64(int64_t a);
int64_t neon_f64_to_i64(double x);
neon_str neon_f64_to_fixed(double x, int64_t places);

// ---- str ----
neon_str neon_str_lit(const char* data, size_t len); // owner == NULL, static
bool neon_str_eq(neon_str a, neon_str b);             // borrows both
int neon_str_cmp(neon_str a, neon_str b);             // borrows both; -1/0/1, bytewise
neon_str neon_str_concat(neon_str a, neon_str b);     // consumes both
neon_str neon_str_add(neon_str a, neon_str b);        // borrows both (the `+` operator)

// String natives from `std::string`. Following the IR's native-call convention, each
// consumes its `str` arguments (releasing them) and returns a fresh owned value.
int64_t neon_str_byte_len(neon_str s);
bool neon_str_is_empty(neon_str s);
neon_str neon_str_to_upper(neon_str s);
neon_str neon_str_to_lower(neon_str s);
neon_str neon_str_repeat(neon_str s, int64_t n);
bool neon_str_contains(neon_str s, neon_str needle);
bool neon_str_starts_with(neon_str s, neon_str prefix);
bool neon_str_ends_with(neon_str s, neon_str suffix);

// Unchecked primitives behind `std::string`'s checked wrappers. A native cannot build the
// tagged result a throwing function returns, nor an `i64 | null`, nor construct an
// `IndexError` — all are program-specific layouts codegen owns — so the check and the
// error live in Neon and these do the raw work.
neon_str neon_str_slice_unchecked(neon_str s, int64_t from, int64_t to);
neon_str neon_str_char_at_unchecked(neon_str s, int64_t i);
int64_t neon_str_index_of(neon_str s, neon_str needle); // -1 when absent
bool neon_str_is_int(neon_str s);
int64_t neon_str_parse_int(neon_str s);

// ---- list (elements moved in/out by codegen through the void* slot pointer) ----
neon_list* neon_list_new(const neon_witness* w);
neon_list* neon_list_new_with_capacity(const neon_witness* w, int64_t cap);
int64_t neon_list_len(neon_list* l);                        // consumes l
void* neon_list_at(neon_list* l, int64_t i); // borrows l; slot pointer, traps OOB
neon_list* neon_list_push(neon_list* l, const void* elem);  // consumes l, moves *elem in
neon_list* neon_list_set(neon_list* l, int64_t i, const void* elem); // consumes l, traps OOB
neon_list* neon_list_concat(neon_list* a, neon_list* b);    // consumes both
int neon_list_cmp(const neon_list* a, const neon_list* b);  // borrows both; -1/0/1
bool neon_list_eq(const neon_list* a, const neon_list* b);  // borrows both
neon_str neon_str_join(neon_list* parts, neon_str sep);     // consumes both; List[str] -> str

// ---- map ----
neon_map* neon_map_new(const neon_key_witness* kw, const neon_witness* vw);
int64_t neon_map_len(neon_map* m);                                  // consumes m
// `contains` and `set` *consume* their key, like any other native: `set` moves it into the
// table, or drops it when the table already holds that key. `at` and `find` borrow it --
// they are reached through `Op::Index`, whose operands the refcount pass releases itself,
// so releasing here too would double-free.
bool neon_map_contains(neon_map* m, const void* key);               // consumes m and key
neon_map* neon_map_set(neon_map* m, const void* key, const void* val); // consumes m and key
void* neon_map_at(neon_map* m, const void* key);   // borrows both; traps if absent
void* neon_map_find(neon_map* m, const void* key); // borrows both; NULL when absent
bool neon_map_eq(neon_map* a, neon_map* b);        // borrows both; same keys, equal values
neon_map* neon_map_remove(neon_map* m, const void* key); // consumes m and key
neon_list* neon_map_keys(neon_map* m, const neon_witness* w);   // consumes m
neon_list* neon_map_values(neon_map* m, const neon_witness* w); // consumes m

// ---- any: the one erasure boundary ----
//
// A boxed value: the object header, the payload's value-witness (its size and how to
// release it), a type tag identifying the concrete type for `is`/`as`, and then the
// payload bytes. `neon_value` is a pointer to one of these.
typedef struct neon_box {
    neon_header header;
    const neon_witness* w;
    uint64_t type_tag;
} neon_box;

neon_value neon_box_new(const void* payload, const neon_witness* w, uint64_t tag);

static inline uint64_t neon_box_tag(neon_value v) {
    return ((neon_box*)v)->type_tag;
}
static inline void* neon_box_payload(neon_value v) {
    return (void*)((neon_box*)v + 1);
}

// ---- natives the corpus calls ----
neon_str neon_i64_to_string(int64_t n);
neon_str neon_f64_to_string(double x);
neon_str neon_bool_to_string(bool b);
neon_str neon_str_to_string(neon_str s);
void neon_io_println(neon_str s); // consumes s

#endif
