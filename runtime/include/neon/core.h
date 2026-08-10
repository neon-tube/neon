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
//
// `rc` is `uint32_t`, not `uint64_t`, and it is load-bearing for size: with `flags` it
// fills the eight bytes before the (8-byte-aligned) `drop` pointer exactly, so the header
// is 16 bytes rather than 24 — a third off every heap object, and a Node drops from a
// 48-byte slab class to 32. Four billion references to ONE object is not a real number:
// each reference is a pointer stored somewhere, so reaching 2^32 would take 32 GB of
// pointers to a single node. The `drop` pointer is kept a real pointer, deliberately —
// folding it into a type index would trade eight bytes for a table lookup on the release
// path, which is a quarter of binary-trees' run, exactly the indirection the drop-
// devirtualisation experiment showed does not pay.
typedef struct neon_header {
    uint32_t rc;
    uint32_t flags;
    void (*drop)(void*);
} neon_header;

// An object whose header carries this flag ignores retain and release entirely.
// Nothing in the shipping runtime sets it yet: a string literal is a view with
// `owner == NULL` (below) and never owns a header at all, so today the flag has no
// producer. It is kept — and its exact semantics are proved by the model
// `an-immortal-object-is-never-dropped` — as the mechanism for future truly-static
// heap objects (interned strings, static containers), whose headers would set it at
// construction and never be freed.
#define NEON_IMMORTAL 1u

// The slab allocator stashes each block's size class in the header's `flags`, so
// `neon_free` can find the class from the pointer alone with no per-object overhead:
// bits 8..15 hold `class + 1` (a zeroed flags word is thus never a valid class), and
// bit 16 marks a block that came straight from `malloc` rather than a slab. Set by
// `neon_alloc`, read by `neon_free`; see `src/lifecycle.c`. Bit 0 (`NEON_IMMORTAL`)
// stays free of these.
#define NEON_ALLOC_CLASS_SHIFT 8
#define NEON_ALLOC_CLASS_MASK 0xff00u
#define NEON_ALLOC_BIG 0x10000u

// Bit 17 marks an object allocated from a per-fiber arena rather than the global slab (see
// docs/design/fibers.md and neon/arena.h). `neon_alloc` sets it when a fiber's arena is
// current; `neon_free` reads it to route the free back to that arena instead of the slab.
// The class bits (8..15) and BIG (16) keep their meaning alongside it — an arena still uses
// the same size-class scheme — so an arena block carries ARENA plus either a class or BIG.
#define NEON_ALLOC_ARENA 0x20000u

// Bit 18 marks a freed arena slot. An arena is WALKABLE (docs/design/fibers.md's teardown
// walk): to step slot-by-slot the walk reads each slot's class from `flags`, so a freed slot
// must keep its class bits and merely flag itself dead rather than clobbering `flags` with a
// free-list link. The link lives in the `drop` field instead (offset 8), which a freed slot
// no longer needs. Set by `neon_arena_free`, cleared when the slot is reallocated, read by
// `neon_arena_walk` to skip the dead.
#define NEON_ALLOC_FREED 0x40000u

// A string is a view: a data pointer and length (the pair libc wants), plus the
// refcounted allocation it points into. A literal has owner == NULL: static, never freed.
typedef struct {
    char* data;
    size_t len;
    neon_header* owner;
} neon_str;

// Below this many bytes, a plain loop beats calling into libc.
//
// `memcmp` and `memcpy` resolve through an ifunc to a vectorised implementation that is
// superb on long buffers and, on short ones, spends most of its time being called. Measured
// on this machine, byte-loop against `memcmp` at 60M iterations per length: the loop wins at
// 4-6 bytes, ties at 7-8, and loses steadily after -- 0.136s against 0.101s by 12 bytes, and
// far worse beyond. So the boundary is 6, not a round number chosen for looking tidy.
//
// This matters because short strings are not a corner case here: a map key, a formatted
// integer and an interpolation fragment are all a handful of bytes, and on the
// word-frequency benchmark `memcmp` called from `neon_map_slot` was 16.5% of the entire run
// -- nearly all of it call overhead, comparing five bytes.
//
// Long strings keep the vectorised path. This is a fast path, not a replacement.
#define NEON_STR_SHORT 6

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
// `copy` deep-relocates one element: read the value at `src`, build an independent copy of
// it — fresh allocations for every heap part, made through `neon_alloc` so the AMBIENT
// routing decides where they land — and write it to `dst`. NULL when a `memcpy` of `size`
// bytes IS an independent copy (scalar-only elements). This is what lets a value cross
// fibers: the send path routes allocation to the shared heap, runs `copy`, and the result
// owes nothing to the sender's arena. A type that cannot cross (a closure, a resource)
// gets a copy that traps, never NULL — silence would corrupt, loudness names the limit.
// Trailing member on purpose: an initializer that stops at `cmp` leaves it NULL.
typedef struct neon_witness {
    size_t size;
    void (*retain)(void* elem);
    void (*release)(void* elem);
    bool (*eq)(const void* a, const void* b);
    int (*cmp)(const void* a, const void* b);
    void (*copy)(const void* src, void* dst);
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
