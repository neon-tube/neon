#ifndef NEON_URL_H
#define NEON_URL_H

// The uriparser seam behind `std::url`: RFC 3986 parsing, components handed back as owned
// strings, raw (still percent-encoded — a request-target must stay encoded on the wire;
// `decode` is the explicit step). Consumes its arguments like every native.

#include <stdbool.h>
#include <stdint.h>

#include "neon/core.h"

// False when `s` is not an RFC 3986 URI; the out-params are written only on success.
// `port` is -1 when absent. `path` is the raw path ("" when empty), `query`/`fragment`
// without their '?'/'#' ("" when absent).
bool neon_url_parse(neon_str s, neon_str* scheme, neon_str* host, int64_t* port,
                    neon_str* path, neon_str* query, neon_str* fragment);

// Percent-decoding, on a copy. '+' is left alone — form encoding is a different, explicit
// thing.
neon_str neon_url_decode(neon_str s);

#endif
