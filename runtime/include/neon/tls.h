#ifndef NEON_TLS_H
#define NEON_TLS_H

// TLS — the primitives behind `net::tls`, mbedTLS under `std::net`'s fiber-parking
// descriptors. See `src/tls.c` for the design; the API's shape follows `neon/net.h`: an
// `int64_t` return is a handle or a negative code, and a native that produces data *and*
// can fail uses out-parameters, which codegen hands back to Neon as a tuple.
//
// Two kinds of handle, both opaque `int64_t`s minted here and owned by a stdlib
// `Resource`: a STREAM (one encrypted connection: ssl context + descriptor) and a server
// LISTEN CONFIG (the certificate chain and private key a listener answers with). The
// handshake natives take ownership of the descriptor they are given — on failure the fd is
// closed, so no path leaks a socket between `net::tcp`'s accept/connect and the guard.
//
// Failures at the handshake boundary carry a rendered MESSAGE out-parameter rather than a
// bare code, because the interesting failures (an unverifiable certificate, a missing
// trust store) are not spellable as an errno and the flags that explain them are only
// readable while the ssl context is still alive. Read/write return mbedTLS codes, which
// `neon_tls_strerror` renders — the `neon_io_strerror` pattern, one library over.

#include <stdbool.h>
#include <stdint.h>

#include "neon/core.h"

// Wrap a connected descriptor as a TLS client and run the handshake. `host` is the name
// SNI announces and — when `verify` is true — the name the peer's certificate must match;
// verification is the full chain against the OS trust store (or $SSL_CERT_FILE /
// $SSL_CERT_DIR), MBEDTLS_SSL_VERIFY_REQUIRED. Returns a stream handle, or a negative
// code with `*err` naming what failed. Consumes `host`; owns `fd` either way.
int64_t neon_tls_client(int64_t fd, neon_str host, bool verify, neon_str* err);

// Load a certificate chain and private key (both PEM files) for a server. Returns a listen
// config handle, or a negative code with `*err` naming the file that refused. Consumes
// both paths.
int64_t neon_tls_listen_config(neon_str cert_path, neon_str key_path, neon_str* err);

// Wrap an accepted descriptor as a TLS server and run the handshake, answering with
// `cfg`'s certificate. Returns a stream handle, or a negative code with `*err`. Owns `fd`
// either way; `cfg` stays usable for the next accept.
int64_t neon_tls_accept(int64_t cfg, int64_t fd, neon_str* err);

// Up to `max` decrypted bytes, waiting until some arrive. An empty result with `*err == 0`
// is EOF: the peer sent close_notify or closed its end.
neon_str neon_tls_read(int64_t h, int64_t max, int64_t* err);

// Every byte, encrypted, waiting whenever the socket buffer fills. 0 or a negative code.
// Consumes `data`.
int64_t neon_tls_write(int64_t h, neon_str data);

// close_notify (best-effort), free the TLS state, close the descriptor.
int64_t neon_tls_close(int64_t h);

// Free a listen config.
int64_t neon_tls_config_free(int64_t cfg);

// Render a code from `read`/`write` — mbedTLS's own description.
neon_str neon_tls_strerror(int64_t code);

#endif
