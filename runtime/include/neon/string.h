#ifndef NEON_STRING_H
#define NEON_STRING_H

// `str` operations, the `std::string` natives, and the `to_string` conversions.
//
// The conversions (`neon_i64_to_string` and friends) were filed under "natives the corpus
// calls" in the old single header; they are folded in here because they are string
// *constructors*, they live in `string.c`, and separating a header for them would put
// `neon_str_to_string` in a different file from every other `neon_str_*`.

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "neon/core.h"
#include "neon/list.h"

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
int64_t neon_str_byte_at_unchecked(neon_str s, int64_t i); // 0..=255, never negative
int64_t neon_str_index_of(neon_str s, neon_str needle); // -1 when absent
bool neon_str_is_int(neon_str s);
int64_t neon_str_parse_int(neon_str s);
bool neon_str_is_float(neon_str s);      // strtod's grammar, whole string, no trimming
double neon_str_parse_float(neon_str s);

neon_str neon_str_join(neon_list* parts, neon_str sep); // consumes both; List[str] -> str

// Conversions to `str`.
neon_str neon_i64_to_string(int64_t n);
neon_str neon_f64_to_string(double x);
neon_str neon_bool_to_string(bool b);
neon_str neon_str_to_string(neon_str s);

#endif
