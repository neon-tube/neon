#ifndef NEON_ENCODING_H
#define NEON_ENCODING_H

// The table-and-byte primitives behind `std::encoding`; the loops and errors are Neon.

#include <stdint.h>

#include "neon/core.h"

neon_str neon_byte_to_hex(int64_t b);   // a byte as two lowercase hex digits
int64_t neon_hex_digit(int64_t b);      // 0..=15, or -1 if not a hex digit
neon_str neon_byte_to_str(int64_t b);   // a byte value as a one-byte str
neon_str neon_base64_char(int64_t v);   // 6-bit value to a base64 character
int64_t neon_base64_value(int64_t b);   // 0..=63, -1 not in alphabet, -2 for '='

#endif
