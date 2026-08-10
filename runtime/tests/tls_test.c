// `runtime/src/tls.c`: mbedTLS over the fiber BIO seam. What this pins: the loopback
// handshake actually completes through our EAGAIN-wait-retry callbacks (client and server
// side at once, over a socketpair, off-fiber — where `neon_net_wait` is a plain poll, the
// same loop a fiber would park in), bytes cross encrypted and come out intact, the
// verify-by-default policy (a trusted CA passes, a hostname mismatch refuses loudly), and
// the credential-loading errors name their file. The corpus test drives the same natives
// ON fibers; llhttp-style, mbedTLS's own correctness is its upstream suite's problem.
//
// POSIX only, like the natives (Windows is the ENOSYS stub half of tls.c). The two-sided
// handshake needs a second thread — both sides block by design — and tinyunit's forked
// children may spawn threads freely.

#ifndef _WIN32

#include <pthread.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#include "tinyunit.h"

#include "support.h"

TEST_SUITE("tls");

// A self-signed ECDSA P-256 certificate for CN=localhost (SAN: DNS:localhost,
// IP:127.0.0.1), valid to 2126 — a fixture, not a secret. Self-signed means it is its own
// CA: pointing $SSL_CERT_FILE at it makes the verified-client path testable with no trust
// store on the machine.
static const char nt_tls_cert[] =
    "-----BEGIN CERTIFICATE-----\n"
    "MIIBmzCCAUGgAwIBAgIUf/KUFZPT4bPKcDEnCHQl68TEubEwCgYIKoZIzj0EAwIw\n"
    "FDESMBAGA1UEAwwJbG9jYWxob3N0MCAXDTI2MDgxMDE5MDAyNloYDzIxMjYwNzE3\n"
    "MTkwMDI2WjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjO\n"
    "PQMBBwNCAAQUOrsbrJxd81yaCvtRdK3wG+RMjEvzMYSQc5sUHQilnn8xZE9H8bKp\n"
    "zmU4TXF2jyOyMU8+pj8LDtKgYmkSsO2qo28wbTAdBgNVHQ4EFgQU5JwDxe77swcw\n"
    "nG+ok6kPI3bZ7aowHwYDVR0jBBgwFoAU5JwDxe77swcwnG+ok6kPI3bZ7aowDwYD\n"
    "VR0TAQH/BAUwAwEB/zAaBgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwCgYIKoZI\n"
    "zj0EAwIDSAAwRQIgQPsQQtz0KtOG8060eCd2JKlG0xnycpBAS4/oGIMNgogCIQCG\n"
    "eQPwKUvvXZ/x8XmGHtMrolMwMueBUPZJrvSnwLYDyg==\n"
    "-----END CERTIFICATE-----\n";

static const char nt_tls_key[] =
    "-----BEGIN PRIVATE KEY-----\n"
    "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgovH2t95/u9U4l4kJ\n"
    "iKpiVUcCvvJpJcwDb6DMiM6qzsWhRANCAAQUOrsbrJxd81yaCvtRdK3wG+RMjEvz\n"
    "MYSQc5sUHQilnn8xZE9H8bKpzmU4TXF2jyOyMU8+pj8LDtKgYmkSsO2q\n"
    "-----END PRIVATE KEY-----\n";

static neon_str lit(const char* s) {
    return neon_str_lit(s, strlen(s));
}

// The fixture PEMs written to files in the working directory (the natives take paths, as
// the stdlib does), removed by each test on its way out.
static void nt_tls_fixture(char* cert_path, char* key_path) {
    nt_temp_path(cert_path, "tls_cert");
    nt_temp_path(key_path, "tls_key");
    FILE* f = fopen(cert_path, "wb");
    fwrite(nt_tls_cert, 1, sizeof(nt_tls_cert) - 1, f);
    fclose(f);
    f = fopen(key_path, "wb");
    fwrite(nt_tls_key, 1, sizeof(nt_tls_key) - 1, f);
    fclose(f);
}

// ---- credential loading fails at the right line, with the file named ----

TEST(listen_config_names_a_missing_file) {
    neon_str err;
    int64_t cfg = neon_tls_listen_config(lit("/no/such/cert.pem"), lit("/no/such/key.pem"),
                                         &err);
    EXPECT(cfg < 0);
    EXPECT(neon_str_len(&err) > 0);
    EXPECT(memcmp(neon_str_data(&err), "cannot read the certificate", 27) == 0);
    neon_str_release(err);
}

TEST(listen_config_rejects_a_garbled_certificate) {
    char cert_path[NT_PATH_MAX];
    char key_path[NT_PATH_MAX];
    nt_tls_fixture(cert_path, key_path);
    FILE* f = fopen(cert_path, "wb");
    fputs("this is not PEM\n", f);
    fclose(f);
    neon_str err;
    int64_t cfg = neon_tls_listen_config(lit(cert_path), lit(key_path), &err);
    EXPECT(cfg < 0);
    EXPECT(neon_str_len(&err) > 0); // the message carries mbedTLS's parse diagnosis
    neon_str_release(err);
    remove(cert_path);
    remove(key_path);
}

// ---- the loopback: both sides of a live handshake over the BIO callbacks ----
//
// A socketpair, the server side on its own thread (a handshake blocks until the peer
// answers, so one thread cannot run both), the client on this one. Every byte of the
// handshake and the application data crosses the non-blocking descriptors through
// `neon_tls_bio_send`/`_recv`'s wait-and-retry — precisely the loop a fiber parks in.

typedef struct {
    int fd;
    const char* cert_path;
    const char* key_path;
    int64_t stream; // out: the accepted stream, or the negative failure
    char echoed[16];
} nt_tls_server;

static void* nt_tls_server_main(void* arg) {
    nt_tls_server* sv = (nt_tls_server*)arg;
    neon_str err;
    int64_t cfg = neon_tls_listen_config(lit(sv->cert_path), lit(sv->key_path), &err);
    neon_str_release(err);
    if (cfg < 0) {
        sv->stream = cfg;
        return NULL;
    }
    sv->stream = neon_tls_accept(cfg, sv->fd, &err);
    neon_str_release(err);
    if (sv->stream >= 0) {
        int64_t rerr = 0;
        neon_str got = neon_tls_read(sv->stream, 16, &rerr);
        size_t n = neon_str_len(&got);
        memcpy(sv->echoed, neon_str_data(&got), n < sizeof(sv->echoed) ? n : 0);
        neon_str_release(got);
        neon_tls_write(sv->stream, lit("pong"));
        neon_tls_close(sv->stream);
        sv->stream = 0;
    }
    neon_tls_config_free(cfg);
    return NULL;
}

TEST(a_loopback_handshake_carries_bytes_both_ways) {
    char cert_path[NT_PATH_MAX];
    char key_path[NT_PATH_MAX];
    nt_tls_fixture(cert_path, key_path);
    int pair[2];
    EXPECT_EQ(socketpair(AF_UNIX, SOCK_STREAM, 0, pair), 0);

    nt_tls_server sv = {pair[1], cert_path, key_path, -1, {0}};
    pthread_t th;
    pthread_create(&th, NULL, nt_tls_server_main, &sv);

    // Insecure on purpose: this test pins the transport, the next two pin verification.
    neon_str err;
    int64_t c = neon_tls_client(pair[0], lit("localhost"), false, &err);
    EXPECT(c >= 0);
    neon_str_release(err);
    EXPECT_EQ(neon_tls_write(c, lit("ping")), 0);
    int64_t rerr = 0;
    neon_str reply = neon_tls_read(c, 16, &rerr);
    EXPECT_EQ(rerr, 0);
    EXPECT(nt_str_is(reply, "pong"));
    neon_str_release(reply);
    neon_tls_close(c);

    pthread_join(th, NULL);
    EXPECT_EQ(sv.stream, 0); // the server side handshook, read, and answered
    EXPECT(strcmp(sv.echoed, "ping") == 0);
    remove(cert_path);
    remove(key_path);
}

// Verification ON, trusting the fixture as its own CA via $SSL_CERT_FILE (the OpenSSL
// convention tls.c honours ahead of the system bundles — which is also what keeps this
// test independent of whatever the host has installed). The hostname matches the
// certificate's SAN, so the strict path succeeds end to end.
TEST(a_verified_handshake_succeeds_against_a_trusted_ca) {
    char cert_path[NT_PATH_MAX];
    char key_path[NT_PATH_MAX];
    nt_tls_fixture(cert_path, key_path);
    setenv("SSL_CERT_FILE", cert_path, 1);
    int pair[2];
    EXPECT_EQ(socketpair(AF_UNIX, SOCK_STREAM, 0, pair), 0);

    nt_tls_server sv = {pair[1], cert_path, key_path, -1, {0}};
    pthread_t th;
    pthread_create(&th, NULL, nt_tls_server_main, &sv);

    neon_str err;
    int64_t c = neon_tls_client(pair[0], lit("localhost"), true, &err);
    EXPECT(c >= 0);
    neon_str_release(err);
    EXPECT_EQ(neon_tls_write(c, lit("ping")), 0);
    int64_t rerr = 0;
    neon_str reply = neon_tls_read(c, 16, &rerr);
    EXPECT(nt_str_is(reply, "pong"));
    neon_str_release(reply);
    neon_tls_close(c);
    pthread_join(th, NULL);
    EXPECT_EQ(sv.stream, 0);
    remove(cert_path);
    remove(key_path);
}

// The same trusted CA, the WRONG name: the handshake must refuse — hostname verification
// is part of the default, not an extra — and the error must say why in x509's words
// rather than a bare code.
TEST(a_hostname_mismatch_refuses_the_handshake) {
    char cert_path[NT_PATH_MAX];
    char key_path[NT_PATH_MAX];
    nt_tls_fixture(cert_path, key_path);
    setenv("SSL_CERT_FILE", cert_path, 1);
    int pair[2];
    EXPECT_EQ(socketpair(AF_UNIX, SOCK_STREAM, 0, pair), 0);

    nt_tls_server sv = {pair[1], cert_path, key_path, -1, {0}};
    pthread_t th;
    pthread_create(&th, NULL, nt_tls_server_main, &sv);

    neon_str err;
    int64_t c = neon_tls_client(pair[0], lit("wrong.example"), true, &err);
    EXPECT(c < 0);
    EXPECT(neon_str_len(&err) > 0);
    // The verify flags, rendered: "...(CN) does not match with the expected CN..."
    bool named = false;
    const char* d = neon_str_data(&err);
    for (size_t i = 0; i + 5 <= neon_str_len(&err); i++) {
        if (memcmp(d + i, "match", 5) == 0) {
            named = true;
            break;
        }
    }
    EXPECT(named);
    neon_str_release(err);

    pthread_join(th, NULL);
    remove(cert_path);
    remove(key_path);
}

#endif
