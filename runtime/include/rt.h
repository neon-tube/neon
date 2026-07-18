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
typedef struct neon_list neon_list;
typedef struct neon_map neon_map;
typedef struct { void* fn; neon_header* env; } neon_closure;
typedef void* neon_value;

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

// ---- str ----
neon_str neon_str_lit(const char* data, size_t len); // owner == NULL, static
bool neon_str_eq(neon_str a, neon_str b);             // borrows both
neon_str neon_str_concat(neon_str a, neon_str b);     // consumes both

// ---- natives the corpus calls ----
neon_str neon_i64_to_string(int64_t n);
neon_str neon_f64_to_string(double x);
neon_str neon_bool_to_string(bool b);
neon_str neon_str_to_string(neon_str s);
void neon_io_println(neon_str s); // consumes s

#endif
