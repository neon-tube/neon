// TLS: mbedTLS on the fiber BIO seam — the primitives behind `net::tls`.
//
// The whole trick is that there is no trick. Our sockets are blocking FROM THE FIBER'S
// VIEW: an operation that would block waits in `neon_net_wait`, which parks the calling
// fiber (or polls, off-runtime) and retries. So mbedTLS runs in its ordinary BLOCKING
// configuration — the BIO callbacks below loop on EAGAIN around that same wait and never
// return WANT_READ/WANT_WRITE — and every piece of handshake and record machinery works
// unmodified while the scheduler serves other fibers through every network pause. No
// async TLS state machine, no second API; the seam net.c built is exactly wide enough.
//
// Policy, decided at design time (docs/design/stdlib.md's "loud opt-out" stance):
//
//   * Verification is ON by default — MBEDTLS_SSL_VERIFY_REQUIRED, full chain plus
//     hostname (mbedtls_ssl_set_hostname does SNI and the name check in one). The
//     insecure path exists but is a separate native flag the stdlib names loudly.
//   * CA roots come from the OS at RUNTIME — mbedTLS ships none. $SSL_CERT_FILE /
//     $SSL_CERT_DIR are honoured first (the OpenSSL convention, and what lets a test
//     trust its own throwaway CA), then the well-known bundle locations, then the
//     /etc/ssl/certs directory. No store and verification on is a HANDSHAKE ERROR naming
//     the probe list — never a silent downgrade to no-verify.
//   * Entropy: one process-wide CTR_DRBG seeded once and guarded by a mutex. Thread-local
//     would dodge the mutex, but a scheduler thread's TLS dies with the thread and its
//     drbg would read as a leak to LSan under the corpus; the mutex is held for
//     microseconds of raw RNG, never across a wait, so no fiber parks while holding it.
//
// A server LISTEN CONFIG holds the certificate chain and key as PEM BYTES, parsed afresh
// for every accepted connection rather than shared parsed. Deliberate: a parsed
// mbedtls_pk_context caches RSA blinding state and is not safe under two concurrent
// handshakes on two scheduler threads, and the obvious fix — a mutex across the handshake
// — deadlocks a single-threaded runtime the moment the first handshake parks its fiber
// holding it. Re-parsing a key is microseconds against a network round trip; isolation is
// the cheap correct thing here, the same reasoning as the per-fiber arenas.
//
// Windows: `-ENOSYS` stubs, net.c's honest refusal, same reason.

#include "libneon_rt.h"

#include "internal.h"
#include "platform.h"

#include <errno.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32

int64_t neon_tls_client(int64_t fd, neon_str host, bool verify, neon_str* err) {
    (void)fd;
    (void)verify;
    neon_str_release(host);
    *err = neon_str_lit("", 0);
    return -ENOSYS;
}
int64_t neon_tls_listen_config(neon_str cert_path, neon_str key_path, neon_str* err) {
    neon_str_release(cert_path);
    neon_str_release(key_path);
    *err = neon_str_lit("", 0);
    return -ENOSYS;
}
int64_t neon_tls_accept(int64_t cfg, int64_t fd, neon_str* err) {
    (void)cfg;
    (void)fd;
    *err = neon_str_lit("", 0);
    return -ENOSYS;
}
neon_str neon_tls_read(int64_t h, int64_t max, int64_t* err) {
    (void)h;
    (void)max;
    *err = -ENOSYS;
    return neon_str_lit("", 0);
}
int64_t neon_tls_write(int64_t h, neon_str data) {
    (void)h;
    neon_str_release(data);
    return -ENOSYS;
}
int64_t neon_tls_close(int64_t h) {
    (void)h;
    return -ENOSYS;
}
int64_t neon_tls_config_free(int64_t cfg) {
    (void)cfg;
    return -ENOSYS;
}
neon_str neon_tls_strerror(int64_t code) {
    (void)code;
    return neon_str_lit("not supported on this platform", 30);
}

#else

#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <sys/socket.h>
#include <unistd.h>

#include <mbedtls/ctr_drbg.h>
#include <mbedtls/entropy.h>
#include <mbedtls/error.h>
#include <mbedtls/net_sockets.h> // the MBEDTLS_ERR_NET_* codes only; its sockets are unused
#include <mbedtls/pk.h>
#include <mbedtls/ssl.h>
#include <mbedtls/x509_crt.h>

// ---- the RNG: one drbg, seeded once, mutex-guarded ----

static pthread_once_t neon_tls_rng_once = PTHREAD_ONCE_INIT;
static pthread_mutex_t neon_tls_rng_mutex = PTHREAD_MUTEX_INITIALIZER;
static mbedtls_entropy_context neon_tls_entropy;
static mbedtls_ctr_drbg_context neon_tls_drbg;
static int neon_tls_rng_rc = 0;

static void neon_tls_rng_seed(void) {
    static const char pers[] = "neon net::tls";
    mbedtls_entropy_init(&neon_tls_entropy);
    mbedtls_ctr_drbg_init(&neon_tls_drbg);
    neon_tls_rng_rc =
        mbedtls_ctr_drbg_seed(&neon_tls_drbg, mbedtls_entropy_func, &neon_tls_entropy,
                              (const unsigned char*)pers, sizeof(pers) - 1);
}

// The f_rng every config gets. The mutex covers raw drbg output and the occasional
// reseed — CPU work in the microseconds — and nothing inside can reach a fiber park, so
// it cannot be held across a scheduler switch.
static int neon_tls_rng(void* ctx, unsigned char* out, size_t len) {
    (void)ctx;
    pthread_mutex_lock(&neon_tls_rng_mutex);
    int r = mbedtls_ctr_drbg_random(&neon_tls_drbg, out, len);
    pthread_mutex_unlock(&neon_tls_rng_mutex);
    return r;
}

// ---- the trust store: probed from the OS, loaded once ----

// The ordered probe: an explicit $SSL_CERT_FILE wins, then the bundle names the major
// families install, then a certs DIRECTORY ($SSL_CERT_DIR or /etc/ssl/certs). Spelled in
// one table so the failure message below can name exactly what was tried.
static const char* const neon_tls_ca_files[] = {
    "/etc/ssl/certs/ca-certificates.crt", // Debian/Ubuntu/Arch
    "/etc/pki/tls/certs/ca-bundle.crt",   // RHEL/Fedora
    "/etc/ssl/cert.pem",                  // Alpine, macOS, the BSDs
};

static pthread_once_t neon_tls_ca_once = PTHREAD_ONCE_INIT;
static mbedtls_x509_crt neon_tls_ca_chain;
static bool neon_tls_ca_loaded = false;

static void neon_tls_ca_load(void) {
    mbedtls_x509_crt_init(&neon_tls_ca_chain);
    // parse_file/parse_path return the number of certificates that failed to parse — a
    // system bundle with a few exotic members still loads, so >= 0 is success here.
    const char* env_file = getenv("SSL_CERT_FILE");
    if (env_file != NULL && *env_file != '\0') {
        // An explicit file is authoritative: load it or load nothing, no silent fallback
        // to a store the caller asked to override.
        neon_tls_ca_loaded = mbedtls_x509_crt_parse_file(&neon_tls_ca_chain, env_file) >= 0;
        return;
    }
    for (size_t i = 0; i < sizeof(neon_tls_ca_files) / sizeof(neon_tls_ca_files[0]); i++) {
        if (mbedtls_x509_crt_parse_file(&neon_tls_ca_chain, neon_tls_ca_files[i]) >= 0) {
            neon_tls_ca_loaded = true;
            return;
        }
    }
    const char* dir = getenv("SSL_CERT_DIR");
    if (dir == NULL || *dir == '\0') {
        dir = "/etc/ssl/certs";
    }
    neon_tls_ca_loaded = mbedtls_x509_crt_parse_path(&neon_tls_ca_chain, dir) >= 0;
}

// ---- handles ----

// One encrypted connection. The ssl context, its config, and — for a server stream — the
// parsed certificate and key it answered with, all owned here and freed together in
// `neon_tls_close`; the global CA chain is referenced, never owned.
typedef struct {
    mbedtls_ssl_context ssl;
    mbedtls_ssl_config conf;
    mbedtls_x509_crt own_cert; // server side; initialized-but-empty on a client
    mbedtls_pk_context own_key;
    int fd;
} neon_tls_stream;

// A server's credentials, held as PEM bytes (NUL-terminated, `len` including it, the form
// the mbedTLS PEM parsers demand) and re-parsed per accept — see the header comment.
typedef struct {
    unsigned char* cert_pem;
    size_t cert_len;
    unsigned char* key_pem;
    size_t key_len;
} neon_tls_listen_cfg;

// ---- small helpers ----

// A NUL-terminated copy of a neon_str (which is length-counted, not terminated), for the
// mbedTLS entry points that want C strings. Caller frees.
static char* neon_tls_cstr(neon_str s) {
    size_t n = neon_str_len(&s);
    char* out = (char*)malloc(n + 1);
    if (out == NULL) {
        neon_trap("out of memory");
    }
    memcpy(out, neon_str_data(&s), n);
    out[n] = '\0';
    return out;
}

// Whole file into a NUL-terminated buffer, `*len` including the NUL. False on any failure.
static bool neon_tls_slurp(const char* path, unsigned char** buf, size_t* len) {
    FILE* f = fopen(path, "rb");
    if (f == NULL) {
        return false;
    }
    if (fseek(f, 0, SEEK_END) != 0) {
        fclose(f);
        return false;
    }
    long size = ftell(f);
    if (size < 0 || fseek(f, 0, SEEK_SET) != 0) {
        fclose(f);
        return false;
    }
    unsigned char* out = (unsigned char*)malloc((size_t)size + 1);
    if (out == NULL) {
        neon_trap("out of memory");
    }
    if (fread(out, 1, (size_t)size, f) != (size_t)size) {
        free(out);
        fclose(f);
        return false;
    }
    fclose(f);
    out[size] = '\0';
    *buf = out;
    *len = (size_t)size + 1;
    return true;
}

// Render an mbedTLS code the way mbedtls_strerror spells it, into a caller's buffer.
static void neon_tls_render(int code, char* buf, size_t cap) {
    mbedtls_strerror(code, buf, cap);
}

// Flatten a multi-line mbedTLS message (verify_info emits one "! reason\n" per flag) into
// the single line a Neon error message wants.
static void neon_tls_one_line(char* s) {
    size_t w = 0;
    bool space = false;
    for (size_t r = 0; s[r] != '\0'; r++) {
        char c = s[r];
        if (c == '\n' || c == '\r' || c == '!') {
            space = w > 0;
            continue;
        }
        if (c == ' ' && (w == 0 || space)) {
            space = w > 0;
            continue;
        }
        if (space) {
            s[w++] = ' ';
            space = false;
        }
        s[w++] = c;
    }
    s[w] = '\0';
}

static int neon_tls_nonblock(int fd) {
    int fl = fcntl(fd, F_GETFL, 0);
    if (fl < 0) {
        return -errno;
    }
    if (fcntl(fd, F_SETFL, fl | O_NONBLOCK) < 0) {
        return -errno;
    }
    return 0;
}

// ---- the BIO seam ----
//
// mbedTLS hands us the exact bytes it wants moved; we move them through the same
// EAGAIN-wait-retry loop net.c's own read and write use. From mbedTLS's point of view
// these callbacks BLOCK — they return only with progress or a hard failure, never
// WANT_READ/WANT_WRITE — because the waiting happens one layer down, where a fiber can do
// it without stopping its thread.

static int neon_tls_bio_send(void* ctx, const unsigned char* buf, size_t len) {
    int fd = ((neon_tls_stream*)ctx)->fd;
    for (;;) {
        // MSG_NOSIGNAL for the same reason as net.c: a vanished peer must be a reportable
        // failure, not a SIGPIPE.
        neon_ssize n = send(fd, buf, len, MSG_NOSIGNAL);
        if (n >= 0) {
            return (int)n;
        }
        if (errno == EINTR) {
            continue;
        }
        if (errno == EAGAIN || errno == EWOULDBLOCK) {
            if (neon_net_wait(fd, true) < 0) {
                return MBEDTLS_ERR_NET_SEND_FAILED;
            }
            continue;
        }
        return MBEDTLS_ERR_NET_SEND_FAILED;
    }
}

static int neon_tls_bio_recv(void* ctx, unsigned char* buf, size_t len) {
    int fd = ((neon_tls_stream*)ctx)->fd;
    for (;;) {
        neon_ssize n = recv(fd, buf, len, 0);
        if (n > 0) {
            return (int)n;
        }
        if (n == 0) {
            return 0; // peer closed; mbedTLS reads this as connection-closed
        }
        if (errno == EINTR) {
            continue;
        }
        if (errno == EAGAIN || errno == EWOULDBLOCK) {
            if (neon_net_wait(fd, false) < 0) {
                return MBEDTLS_ERR_NET_RECV_FAILED;
            }
            continue;
        }
        return MBEDTLS_ERR_NET_RECV_FAILED;
    }
}

// ---- construction and the handshake ----

static neon_tls_stream* neon_tls_stream_new(int fd) {
    neon_tls_stream* s = (neon_tls_stream*)malloc(sizeof(neon_tls_stream));
    if (s == NULL) {
        neon_trap("out of memory");
    }
    mbedtls_ssl_init(&s->ssl);
    mbedtls_ssl_config_init(&s->conf);
    mbedtls_x509_crt_init(&s->own_cert);
    mbedtls_pk_init(&s->own_key);
    s->fd = fd;
    return s;
}

// Free everything a stream owns, closing its descriptor. The teardown for every failure
// path and for `neon_tls_close` alike, so no path can free half of it.
static void neon_tls_stream_free(neon_tls_stream* s) {
    mbedtls_ssl_free(&s->ssl);
    mbedtls_ssl_config_free(&s->conf);
    mbedtls_x509_crt_free(&s->own_cert);
    mbedtls_pk_free(&s->own_key);
    close(s->fd);
    free(s);
}

// Run the handshake to completion. Because the BIO blocks, the WANT_* codes cannot
// actually come back — the loop is the documented shape, kept so a future non-blocking
// BIO does not silently break this. On failure the message names the interesting cause:
// an unverifiable certificate gets the verify flags spelled out (only readable while the
// ssl context is alive, which is why this renders rather than returns them), anything
// else gets mbedtls_strerror.
static int neon_tls_handshake(neon_tls_stream* s, const char* what, neon_str* err) {
    int r;
    while ((r = mbedtls_ssl_handshake(&s->ssl)) != 0) {
        if (r == MBEDTLS_ERR_SSL_WANT_READ || r == MBEDTLS_ERR_SSL_WANT_WRITE) {
            continue;
        }
        char detail[256];
        uint32_t flags = mbedtls_ssl_get_verify_result(&s->ssl);
        if (r == MBEDTLS_ERR_X509_CERT_VERIFY_FAILED && flags != 0 && flags != 0xFFFFFFFFu) {
            mbedtls_x509_crt_verify_info(detail, sizeof(detail), "", flags);
        } else {
            neon_tls_render(r, detail, sizeof(detail));
        }
        neon_tls_one_line(detail);
        char msg[512];
        int n = snprintf(msg, sizeof(msg), "%s: %s", what, detail);
        *err = neon_str_new(msg, n < 0 ? 0 : strlen(msg));
        return r;
    }
    *err = neon_str_lit("", 0);
    return 0;
}

int64_t neon_tls_client(int64_t fd, neon_str host, bool verify, neon_str* err) {
    char* host_c = neon_tls_cstr(host);
    neon_str_release(host);

    pthread_once(&neon_tls_rng_once, neon_tls_rng_seed);
    if (neon_tls_rng_rc != 0) {
        char msg[128];
        snprintf(msg, sizeof(msg), "cannot seed the TLS random generator (-0x%04x)",
                 (unsigned)-neon_tls_rng_rc);
        *err = neon_str_new(msg, strlen(msg));
        free(host_c);
        close((int)fd);
        return neon_tls_rng_rc;
    }
    if (verify) {
        pthread_once(&neon_tls_ca_once, neon_tls_ca_load);
        if (!neon_tls_ca_loaded) {
            // The refusal the design demands: no trust store and verification on is an
            // ERROR that names what was probed, never a quiet downgrade to no-verify.
            const char* msg =
                "no CA trust store found (tried $SSL_CERT_FILE, "
                "/etc/ssl/certs/ca-certificates.crt, /etc/pki/tls/certs/ca-bundle.crt, "
                "/etc/ssl/cert.pem, then $SSL_CERT_DIR or /etc/ssl/certs); "
                "install CA certificates or use the insecure connect to skip verification";
            *err = neon_str_new(msg, strlen(msg));
            free(host_c);
            close((int)fd);
            return -ENOENT;
        }
    }

    // A blocking descriptor would still be CORRECT here (the BIO's send would simply
    // block the thread), but it would defeat the parking that makes TLS concurrent, so
    // make non-blocking unconditional rather than trusting every caller's provenance.
    neon_tls_nonblock((int)fd);

    neon_tls_stream* s = neon_tls_stream_new((int)fd);
    int r = mbedtls_ssl_config_defaults(&s->conf, MBEDTLS_SSL_IS_CLIENT,
                                        MBEDTLS_SSL_TRANSPORT_STREAM,
                                        MBEDTLS_SSL_PRESET_DEFAULT);
    if (r == 0) {
        mbedtls_ssl_conf_authmode(&s->conf, verify ? MBEDTLS_SSL_VERIFY_REQUIRED
                                                   : MBEDTLS_SSL_VERIFY_NONE);
        if (verify) {
            mbedtls_ssl_conf_ca_chain(&s->conf, &neon_tls_ca_chain, NULL);
        }
        mbedtls_ssl_conf_rng(&s->conf, neon_tls_rng, NULL);
        r = mbedtls_ssl_setup(&s->ssl, &s->conf);
    }
    // The hostname is set even on the insecure path: it is also SNI, and a server behind
    // a name-routing frontend answers wrongly (or not at all) without it.
    if (r == 0) {
        r = mbedtls_ssl_set_hostname(&s->ssl, host_c);
    }
    if (r != 0) {
        char detail[256];
        neon_tls_render(r, detail, sizeof(detail));
        char msg[512];
        snprintf(msg, sizeof(msg), "cannot set up TLS for `%s`: %s", host_c, detail);
        *err = neon_str_new(msg, strlen(msg));
        free(host_c);
        neon_tls_stream_free(s);
        return r;
    }
    mbedtls_ssl_set_bio(&s->ssl, s, neon_tls_bio_send, neon_tls_bio_recv, NULL);

    char what[300];
    snprintf(what, sizeof(what), "TLS handshake with `%s` failed", host_c);
    free(host_c);
    r = neon_tls_handshake(s, what, err);
    if (r != 0) {
        neon_tls_stream_free(s);
        return r;
    }
    return (int64_t)(intptr_t)s;
}

int64_t neon_tls_listen_config(neon_str cert_path, neon_str key_path, neon_str* err) {
    char* cert_c = neon_tls_cstr(cert_path);
    char* key_c = neon_tls_cstr(key_path);
    neon_str_release(cert_path);
    neon_str_release(key_path);

    pthread_once(&neon_tls_rng_once, neon_tls_rng_seed);

    neon_tls_listen_cfg* cfg = (neon_tls_listen_cfg*)calloc(1, sizeof(neon_tls_listen_cfg));
    if (cfg == NULL) {
        neon_trap("out of memory");
    }
    char msg[512];
    msg[0] = '\0';
    if (!neon_tls_slurp(cert_c, &cfg->cert_pem, &cfg->cert_len)) {
        snprintf(msg, sizeof(msg), "cannot read the certificate file `%s`", cert_c);
    } else if (!neon_tls_slurp(key_c, &cfg->key_pem, &cfg->key_len)) {
        snprintf(msg, sizeof(msg), "cannot read the private key file `%s`", key_c);
    } else {
        // Parse both once NOW, so a bad file fails at `listen` — where the operator who
        // chose the paths is present — rather than at the first client's handshake.
        mbedtls_x509_crt crt;
        mbedtls_pk_context pk;
        mbedtls_x509_crt_init(&crt);
        mbedtls_pk_init(&pk);
        int r = mbedtls_x509_crt_parse(&crt, cfg->cert_pem, cfg->cert_len);
        if (r != 0) {
            char detail[256];
            neon_tls_render(r, detail, sizeof(detail));
            neon_tls_one_line(detail);
            snprintf(msg, sizeof(msg), "cannot parse the certificate `%s`: %s", cert_c,
                     detail);
        } else {
            r = mbedtls_pk_parse_key(&pk, cfg->key_pem, cfg->key_len, NULL, 0, neon_tls_rng,
                                     NULL);
            if (r != 0) {
                char detail[256];
                neon_tls_render(r, detail, sizeof(detail));
                neon_tls_one_line(detail);
                snprintf(msg, sizeof(msg), "cannot parse the private key `%s`: %s", key_c,
                         detail);
            }
        }
        mbedtls_x509_crt_free(&crt);
        mbedtls_pk_free(&pk);
    }
    free(cert_c);
    free(key_c);
    if (msg[0] != '\0') {
        free(cfg->cert_pem);
        free(cfg->key_pem);
        free(cfg);
        *err = neon_str_new(msg, strlen(msg));
        return -EINVAL;
    }
    *err = neon_str_lit("", 0);
    return (int64_t)(intptr_t)cfg;
}

int64_t neon_tls_accept(int64_t cfg_h, int64_t fd, neon_str* err) {
    neon_tls_listen_cfg* cfg = (neon_tls_listen_cfg*)(intptr_t)cfg_h;

    pthread_once(&neon_tls_rng_once, neon_tls_rng_seed);
    if (neon_tls_rng_rc != 0) {
        char msg[128];
        snprintf(msg, sizeof(msg), "cannot seed the TLS random generator (-0x%04x)",
                 (unsigned)-neon_tls_rng_rc);
        *err = neon_str_new(msg, strlen(msg));
        close((int)fd);
        return neon_tls_rng_rc;
    }
    neon_tls_nonblock((int)fd);

    neon_tls_stream* s = neon_tls_stream_new((int)fd);
    // The per-stream parse of the shared PEM bytes — the concurrency ruling in the header
    // comment. `listen_config` already proved both parse, so a failure here is news.
    int r = mbedtls_x509_crt_parse(&s->own_cert, cfg->cert_pem, cfg->cert_len);
    if (r == 0) {
        r = mbedtls_pk_parse_key(&s->own_key, cfg->key_pem, cfg->key_len, NULL, 0,
                                 neon_tls_rng, NULL);
    }
    if (r == 0) {
        r = mbedtls_ssl_config_defaults(&s->conf, MBEDTLS_SSL_IS_SERVER,
                                        MBEDTLS_SSL_TRANSPORT_STREAM,
                                        MBEDTLS_SSL_PRESET_DEFAULT);
    }
    if (r == 0) {
        // No client certificates asked for: server-authenticates-to-client is the shape
        // `net::tls::listen` promises; mutual TLS is surface for another day.
        mbedtls_ssl_conf_authmode(&s->conf, MBEDTLS_SSL_VERIFY_NONE);
        mbedtls_ssl_conf_rng(&s->conf, neon_tls_rng, NULL);
        r = mbedtls_ssl_conf_own_cert(&s->conf, &s->own_cert, &s->own_key);
    }
    if (r == 0) {
        r = mbedtls_ssl_setup(&s->ssl, &s->conf);
    }
    if (r != 0) {
        char detail[256];
        neon_tls_render(r, detail, sizeof(detail));
        neon_tls_one_line(detail);
        char msg[512];
        snprintf(msg, sizeof(msg), "cannot set up the TLS server side: %s", detail);
        *err = neon_str_new(msg, strlen(msg));
        neon_tls_stream_free(s);
        return r;
    }
    mbedtls_ssl_set_bio(&s->ssl, s, neon_tls_bio_send, neon_tls_bio_recv, NULL);

    r = neon_tls_handshake(s, "TLS accept handshake failed", err);
    if (r != 0) {
        neon_tls_stream_free(s);
        return r;
    }
    return (int64_t)(intptr_t)s;
}

// ---- the stream ----

neon_str neon_tls_read(int64_t h, int64_t max, int64_t* err) {
    neon_tls_stream* s = (neon_tls_stream*)(intptr_t)h;
    *err = 0;
    if (max <= 0) {
        return neon_str_lit("", 0);
    }
    unsigned char* buf = (unsigned char*)malloc((size_t)max);
    if (buf == NULL) {
        neon_trap("out of memory");
    }
    for (;;) {
        int r = mbedtls_ssl_read(&s->ssl, buf, (size_t)max);
        if (r > 0) {
            neon_str out = neon_str_new((const char*)buf, (size_t)r);
            free(buf);
            return out;
        }
        if (r == MBEDTLS_ERR_SSL_WANT_READ || r == MBEDTLS_ERR_SSL_WANT_WRITE) {
            continue; // cannot happen with the blocking BIO; kept for shape
        }
#if defined(MBEDTLS_ERR_SSL_RECEIVED_NEW_SESSION_TICKET)
        if (r == MBEDTLS_ERR_SSL_RECEIVED_NEW_SESSION_TICKET) {
            continue; // TLS 1.3 post-handshake housekeeping, not application data
        }
#endif
        // Both flavours of "the conversation is over" are EOF to the caller: the polite
        // close_notify, and the abrupt TCP close half the internet's servers do instead.
        // The distinction (a truncation attack detector) matters to protocols that frame
        // by connection close with no lengths, which `std::http` is not.
        if (r == 0 || r == MBEDTLS_ERR_SSL_PEER_CLOSE_NOTIFY ||
            r == MBEDTLS_ERR_SSL_CONN_EOF) {
            free(buf);
            return neon_str_lit("", 0);
        }
        *err = r;
        free(buf);
        return neon_str_lit("", 0);
    }
}

int64_t neon_tls_write(int64_t h, neon_str data) {
    neon_tls_stream* s = (neon_tls_stream*)(intptr_t)h;
    const unsigned char* p = (const unsigned char*)neon_str_data(&data);
    size_t left = neon_str_len(&data);
    int64_t status = 0;
    // ssl_write may take fewer bytes than offered (one record's worth); loop like
    // net_write does, so a short write is never the caller's problem.
    while (left > 0) {
        int r = mbedtls_ssl_write(&s->ssl, p, left);
        if (r > 0) {
            p += r;
            left -= (size_t)r;
            continue;
        }
        if (r == MBEDTLS_ERR_SSL_WANT_READ || r == MBEDTLS_ERR_SSL_WANT_WRITE) {
            continue;
        }
        status = r;
        break;
    }
    neon_str_release(data);
    return status;
}

int64_t neon_tls_close(int64_t h) {
    neon_tls_stream* s = (neon_tls_stream*)(intptr_t)h;
    // Best-effort goodbye: a peer that already vanished makes close_notify fail, and
    // that is fine — the descriptor is closing either way.
    mbedtls_ssl_close_notify(&s->ssl);
    neon_tls_stream_free(s);
    return 0;
}

int64_t neon_tls_config_free(int64_t cfg_h) {
    neon_tls_listen_cfg* cfg = (neon_tls_listen_cfg*)(intptr_t)cfg_h;
    free(cfg->cert_pem);
    free(cfg->key_pem);
    free(cfg);
    return 0;
}

neon_str neon_tls_strerror(int64_t code) {
    char buf[256];
    neon_tls_render((int)code, buf, sizeof(buf));
    neon_tls_one_line(buf);
    return neon_str_new(buf, strlen(buf));
}

#endif
