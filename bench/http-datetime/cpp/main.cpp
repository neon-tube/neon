// http-datetime: raw-socket HTTP/1.1 server in C++.
//
// Answers every request with the current UTC time in ISO 8601
// (YYYY-MM-DDTHH:MM:SSZ), computed per request. Keep-alive: one detached
// std::thread per accepted connection, looping until the peer hangs up or
// asks to close. Standard library only.

#include <arpa/inet.h>
#include <csignal>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <ctime>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <sys/socket.h>
#include <thread>
#include <unistd.h>

// Format "now" as YYYY-MM-DDTHH:MM:SSZ into buf. Returns chars written (20).
static int format_now(char *buf, size_t cap) {
    time_t now = time(nullptr);
    struct tm tm;
    gmtime_r(&now, &tm);
    return static_cast<int>(strftime(buf, cap, "%Y-%m-%dT%H:%M:%SZ", &tm));
}

// Read until a full "\r\n\r\n" header terminator is seen.
// Returns true on a complete header, false on EOF/close/error.
static bool read_request(int fd) {
    char buf[4096];
    static const char term[4] = {'\r', '\n', '\r', '\n'};
    int match = 0;
    for (;;) {
        ssize_t n = recv(fd, buf, sizeof(buf), 0);
        if (n <= 0)
            return false;
        for (ssize_t i = 0; i < n; i++) {
            if (buf[i] == term[match]) {
                if (++match == 4)
                    return true;
            } else {
                match = (buf[i] == '\r') ? 1 : 0;
            }
        }
    }
}

static bool write_all(int fd, const char *buf, size_t len) {
    size_t off = 0;
    while (off < len) {
        ssize_t n = send(fd, buf + off, len - off, MSG_NOSIGNAL);
        if (n <= 0)
            return false;
        off += static_cast<size_t>(n);
    }
    return true;
}

static void handle_conn(int fd) {
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
        if (n < 0 || !write_all(fd, resp, static_cast<size_t>(n)))
            break;
    }

    close(fd);
}

int main() {
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
    addr.sin_port = htons(static_cast<uint16_t>(port));

    if (bind(srv, reinterpret_cast<struct sockaddr *>(&addr), sizeof(addr)) != 0) {
        perror("bind");
        return 1;
    }
    if (listen(srv, 1024) != 0) {
        perror("listen");
        return 1;
    }

    for (;;) {
        int fd = accept(srv, nullptr, nullptr);
        if (fd < 0)
            continue;

        try {
            std::thread(handle_conn, fd).detach();
        } catch (...) {
            close(fd);
        }
    }

    return 0;
}
