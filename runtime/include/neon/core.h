#ifndef NEON_CORE_H
#define NEON_CORE_H

// The types every other area is written in terms of, plus the scalar helpers that build
// them. Split out because `neon_str`, the object header and the witnesses are shared by
// nearly every header here: without a common base each of them would have to redeclare
// the others, and the "self-contained, includable in any order" promise would be a
// coincidence rather than a property.
//
// The scalar helpers (`neon_unit_v`, `neon_null`, `neon_atom`, `neon_f64_bits`) live here
// rather than in a header of their own: they are one-line constructors for core types
// with no implementation behind them, and an area file holding four inline functions
// would be an area in name only.

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

// ---- reaching inside a `neon_str` ----
//
// Nothing outside these four should touch `.data` or `.owner`. They are trivial today --
// the layout above is exactly what they return -- and they exist so that a small-string
// optimisation is a change to *them* rather than an audit of every field access in the
// runtime and the backend. See `docs/design/small-strings.md`.
//
// **`neon_str_data` takes a pointer, deliberately.** Under SSO a short string's bytes live
// *inside* the struct, so the address of its data depends on where that struct currently
// is. Taking the string by value would compute the answer for the parameter copy, hand back
// a pointer into it, and leave that pointer dangling the moment the call returned. Passing
// the address makes the caller name the object it means, and makes it visible in the
// signature that the result borrows from that object.
//
// The lifetime rule this implies -- and it is the sharp edge of SSO, not an incidental one:
// **a pointer from `neon_str_data` is valid only while that particular string object is
// alive and has not been copied or moved.** Copy the string and you must re-derive it from
// the copy. `runtime/src/file.c` holds such pointers in an `iovec` across a `writev`; that
// is sound because the array it points into outlives the call, and it is the pattern to
// look at first when this bites.
static inline const char* neon_str_data(const neon_str* s) {
    return s->data;
}

// The mutable form, for a buffer this code just allocated and is filling in. Same lifetime
// rule. Kept separate so that the read-only path, which is nearly every use, cannot hand
// out a writable pointer into a shared string by accident.
static inline char* neon_str_data_mut(neon_str* s) {
    return s->data;
}

static inline size_t neon_str_len(const neon_str* s) {
    return s->len;
}

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

#endif
