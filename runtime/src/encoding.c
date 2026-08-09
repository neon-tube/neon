// The byte-and-table primitives behind `std::encoding`. Each is tiny and `@pure`: a
// table lookup or a one/two-byte string built from a value. The loops and the error
// handling live in Neon; these are only the pieces a byte loop cannot express as a `str`
// operation — turning a numeric byte value into an actual byte, and the two alphabets.

#include "libneon_rt.h"

#include "internal.h"

#include <stdint.h>

static const char HEX[] = "0123456789abcdef";
static const char B64[] = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

// A byte 0..=255 as its two lowercase hex digits, an owned two-byte string.
neon_str neon_byte_to_hex(int64_t b) {
    char buf[2] = {HEX[(b >> 4) & 0xf], HEX[b & 0xf]};
    return neon_str_new(buf, 2);
}

// The value 0..=15 of a hex digit byte, or -1 if it is not one. Upper and lower case.
int64_t neon_hex_digit(int64_t b) {
    if (b >= '0' && b <= '9') {
        return b - '0';
    }
    if (b >= 'a' && b <= 'f') {
        return b - 'a' + 10;
    }
    if (b >= 'A' && b <= 'F') {
        return b - 'A' + 10;
    }
    return -1;
}

// One byte 0..=255 as a one-byte string. The inverse of `string::byte_at`, which the
// language otherwise has no way to spell — every `str` constructor takes text, not a
// numeric byte.
neon_str neon_byte_to_str(int64_t b) {
    char c = (char)(uint8_t)b;
    return neon_str_new(&c, 1);
}

// The base64 character for a 6-bit value 0..=63, standard alphabet.
neon_str neon_base64_char(int64_t v) {
    char c = B64[v & 0x3f];
    return neon_str_new(&c, 1);
}

// The 6-bit value of a base64 character byte: -1 if outside the alphabet, -2 for `=`
// (which the decoder handles as padding, not a value).
int64_t neon_base64_value(int64_t b) {
    if (b >= 'A' && b <= 'Z') {
        return b - 'A';
    }
    if (b >= 'a' && b <= 'z') {
        return b - 'a' + 26;
    }
    if (b >= '0' && b <= '9') {
        return b - '0' + 52;
    }
    if (b == '+') {
        return 62;
    }
    if (b == '/') {
        return 63;
    }
    if (b == '=') {
        return -2;
    }
    return -1;
}
