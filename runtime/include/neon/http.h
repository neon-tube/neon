#ifndef NEON_HTTP_H
#define NEON_HTTP_H

// The llhttp seam behind `std::http`.
//
// llhttp is callback-driven and Neon cannot provide C callbacks, so the wrapper is an
// ACCUMULATOR: feed it bytes, its callbacks copy the interesting spans into growable
// buffers, and once a message is complete the getters hand the parts back as ordinary
// owned values. One parser handles one message at a time; `reset` re-arms it for the next
// message on the same connection (keep-alive), and `feed` reports how many bytes it
// consumed so a pipelined follow-up request is not swallowed.
//
// A parser is an ordinary refcounted runtime object — fiber-local, deliberately
// UNSENDABLE (the connection crosses fibers; its parser is born on the fiber that reads).
// llhttp decodes chunked transfer-encoding into the body callbacks, so `body` is always
// the decoded bytes and `std::http` never sees framing.

#include <stdbool.h>
#include <stdint.h>

#include "neon/core.h"

typedef struct neon_http_parser neon_http_parser;

// Result codes for `feed`, mirrored in std/http.neon.
enum {
    NEON_HTTP_MORE = 0,     // consumed everything; the message needs more bytes
    NEON_HTTP_COMPLETE = 1, // a full message is parsed; getters are valid; reset before
                            // feeding the NEXT message's bytes
    NEON_HTTP_ERROR = 2     // the bytes are not HTTP; `error` names the failure
};

// `response` picks which side of the protocol the parser expects.
neon_http_parser* neon_http_parser_new(bool response);

// Parse `data`. The head is a NEON_HTTP_* code; `*consumed` is how many of `data`'s bytes
// were used (on COMPLETE, bytes after the message's end are NOT consumed — feed them
// again after `reset`). Consumes `data` and the parser reference.
int64_t neon_http_parser_feed(neon_http_parser* p, neon_str data, int64_t* consumed);

// Getters, valid after COMPLETE (before it they return empty/zero values, never trap).
// Each consumes its parser reference, the runtime-wide native convention.
neon_str neon_http_parser_method(neon_http_parser* p);   // requests: "GET", ...
neon_str neon_http_parser_target(neon_http_parser* p);   // requests: the request-target
int64_t neon_http_parser_status(neon_http_parser* p);    // responses: 200, ...
neon_str neon_http_parser_body(neon_http_parser* p);     // decoded body bytes
bool neon_http_parser_keep_alive(neon_http_parser* p);
int64_t neon_http_parser_header_count(neon_http_parser* p);
// The i-th header, in wire order: returns the field name, writes the value. Out of range
// returns empty strings — the count above is the contract.
neon_str neon_http_parser_header_at(neon_http_parser* p, int64_t i, neon_str* value);
// End-of-input: completes an EOF-terminated response body (no Content-Length). Consumes
// the parser reference.
int64_t neon_http_parser_finish(neon_http_parser* p);
// The llhttp error description after NEON_HTTP_ERROR.
neon_str neon_http_parser_error(neon_http_parser* p);
// Re-arm for the next message on the same connection: clears the accumulators, keeps the
// parser's side (request/response).
void neon_http_parser_reset(neon_http_parser* p);

#endif
