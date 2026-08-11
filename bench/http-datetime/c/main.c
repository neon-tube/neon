/* http-datetime: raw-socket HTTP/1.1 server in C.
 *
 * Answers every request with the current UTC time in ISO 8601
 * (YYYY-MM-DDTHH:MM:SSZ), computed per request. Supports keep-alive:
 * one detached pthread per accepted connection, looping until the peer
 * hangs up or asks to close. Standard library only. */

#define _POSIX_C_SOURCE 200809L

#include <arpa/inet.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

/* Format "now" as YYYY-MM-DDTHH:MM:SSZ into buf (needs >= 21 bytes).
 * Returns the number of characters written (20). */
static int format_now(char *buf, size_t cap) {
    time_t now = time(NULL);
    struct tm tm;
    gmtime_r(&now, &tm);
    return (int)strftime(buf, cap, "%Y-%m-%dT%H:%M:%SZ", &tm);
}

/* Read bytes until we have seen a full "\r\n\r\n" header terminator.
 * Returns 1 if a complete request header was read, 0 on EOF/close/error. */
static int read_request(int fd) {
    char buf[4096];
    int match = 0; /* how many chars of "\r\n\r\n" matched so far */
    static const char term[4] = {'\r', '\n', '\r', '\n'};
    for (;;) {
        ssize_t n = recv(fd, buf, sizeof(buf), 0);
        if (n <= 0)
            return 0; /* client closed or error */
        for (ssize_t i = 0; i < n; i++) {
            if (buf[i] == term[match]) {
                if (++match == 4)
                    return 1;
            } else {
                match = (buf[i] == '\r') ? 1 : 0;
            }
        }
    }
}

/* Write all bytes, retrying short writes. Returns 0 on success, -1 on error. */
static int write_all(int fd, const char *buf, size_t len) {
    size_t off = 0;
    while (off < len) {
        ssize_t n = send(fd, buf + off, len - off, MSG_NOSIGNAL);
        if (n <= 0)
            return -1;
        off += (size_t)n;
    }
    return 0;
}

static void *handle_conn(void *arg) {
    int fd = (int)(long)arg;

    int one = 1;
    setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one));

    for (;;) {
        if (!read_request(fd))
            break;

        char ts[32];
        int tslen = format_now(ts, sizeof(ts));

        char resp[256];
        int n = snprintf(resp, sizeof(resp),
                         "HTTP/1.1 200 OK\r\n"
                         "Content-Type: text/plain\r\n"
                         "Content-Length: %d\r\n"
                         "\r\n"
                         "%.*s",
                         tslen, tslen, ts);
        if (n < 0 || write_all(fd, resp, (size_t)n) != 0)
            break;
    }

    close(fd);
    return NULL;
}

int main(void) {
    signal(SIGPIPE, SIG_IGN);

    const char *port_env = getenv("PORT");
    int port = port_env ? atoi(port_env) : 18080;
    if (port <= 0 || port > 65535)
        port = 18080;

    int srv = socket(AF_INET, SOCK_STREAM, 0);
    if (srv < 0) {
        perror("socket");
        return 1;
    }

    int one = 1;
    setsockopt(srv, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));

    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    addr.sin_port = htons((uint16_t)port);

    if (bind(srv, (struct sockaddr *)&addr, sizeof(addr)) != 0) {
        perror("bind");
        return 1;
    }
    if (listen(srv, 1024) != 0) {
        perror("listen");
        return 1;
    }

    for (;;) {
        int fd = accept(srv, NULL, NULL);
        if (fd < 0)
            continue; /* transient accept error: keep serving */

        pthread_t th;
        if (pthread_create(&th, NULL, handle_conn, (void *)(long)fd) != 0) {
            close(fd);
            continue;
        }
        pthread_detach(th);
    }

    return 0;
}
