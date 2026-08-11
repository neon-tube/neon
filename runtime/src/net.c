// Sockets: the primitives behind `std::net`.
//
// Every socket here is NON-BLOCKING at the descriptor level, always, and the blocking is
// reintroduced one layer up by WAITING — which is the whole point, because "wait" is the
// operation a fiber can do without stopping its thread. An operation that would block
// returns EAGAIN, we wait for readiness, and we retry. Inside a fiber that wait parks the
// fiber (the scheduler's engine — io_uring POLL_ADD or epoll — wakes it); off-fiber it is
// an ordinary `poll(2)`. So the same `net::read` blocks the caller and nothing else,
// whether the caller is a fiber or `main`, and the API needs no colour.
//
// The readiness hook is the same trap-handler pattern the rest of the runtime uses: a
// per-thread function pointer the scheduler arms for the life of a runtime, NULL everywhere
// else. This file therefore has no dependency on the fiber sources existing — which matters,
// since they are compiled only on x86-64 SysV.
//
// Windows: every native returns `-ENOSYS`, the same honest refusal `std::process` gives,
// rather than a half-working Winsock port nobody has tested.

#include "libneon_rt.h"

#include "internal.h"
#include "platform.h"

#include <errno.h>
#include <stdlib.h>
#include <string.h>

// The readiness bridge (internal.h). Defined here, in the always-compiled net unit, so a
// build without the fiber sources still links with it permanently NULL.
_Thread_local int (*neon_fiber_readiness_hook)(int fd, bool for_write) = NULL;
_Thread_local int (*neon_fiber_readiness_deadline_hook)(int fd, bool for_write,
                                                        int64_t deadline_ms) = NULL;

// Whether a status is the timeout sentinel. Kept in C — and out of the platform split,
// since it is pure — so the errno value never leaks into Neon.
bool neon_net_is_timeout(int64_t code) {
    return code == -ETIMEDOUT;
}

#ifdef _WIN32

int64_t neon_net_connect_timeout(neon_str addr, int64_t millis) {
    (void)millis;
    return neon_net_connect(addr);
}
neon_str neon_net_read_timeout(int64_t fd, int64_t max, int64_t millis, int64_t* err) {
    (void)millis;
    return neon_net_read(fd, max, err);
}

int64_t neon_net_listen(neon_str addr, int64_t backlog) {
    (void)addr;
    (void)backlog;
    return -ENOSYS;
}
int64_t neon_net_accept(int64_t fd, neon_str* peer) {
    (void)fd;
    *peer = neon_str_lit("", 0);
    return -ENOSYS;
}
int64_t neon_net_connect(neon_str addr) {
    (void)addr;
    return -ENOSYS;
}
neon_str neon_net_read(int64_t fd, int64_t max, int64_t* err) {
    (void)fd;
    (void)max;
    *err = -ENOSYS;
    return neon_str_lit("", 0);
}
int64_t neon_net_write(int64_t fd, neon_list* parts) {
    (void)fd;
    neon_release((neon_header*)parts);
    return -ENOSYS;
}
int64_t neon_net_close(int64_t fd) {
    (void)fd;
    return -ENOSYS;
}
int64_t neon_net_shutdown(int64_t fd, int64_t how) {
    (void)fd;
    (void)how;
    return -ENOSYS;
}
neon_str neon_net_local_addr(int64_t fd, int64_t* err) {
    (void)fd;
    *err = -ENOSYS;
    return neon_str_lit("", 0);
}
int64_t neon_net_set_nodelay(int64_t fd, bool on) {
    (void)fd;
    (void)on;
    return -ENOSYS;
}
int64_t neon_net_udp_bind(neon_str addr) {
    (void)addr;
    return -ENOSYS;
}
int64_t neon_net_udp_send_to(int64_t fd, neon_str addr, neon_str data) {
    (void)fd;
    neon_str_release(addr);
    neon_str_release(data);
    return -ENOSYS;
}
neon_str neon_net_udp_recv_from(int64_t fd, int64_t max, neon_str* peer, int64_t* err) {
    (void)fd;
    (void)max;
    *peer = neon_str_lit("", 0);
    *err = -ENOSYS;
    return neon_str_lit("", 0);
}

#else

#include <fcntl.h>
#include <netdb.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

// ---- waiting ----

// An ABSOLUTE CLOCK_MONOTONIC deadline in milliseconds that bounds every wait until it is
// cleared, or 0 for "wait forever". A net operation is atomic within a fiber — a read does
// not nest inside another read — so a per-fiber thread-local is the whole mechanism: a
// timed entry point stamps it, runs the ordinary connect/read (whose EAGAIN waits honour
// it, including the ones mbedTLS drives through the BIO), and clears it. The hot untimed
// path pays one predictable-not-taken branch.
static _Thread_local int64_t t_net_deadline = 0;

static int64_t neon_net_now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

// Arm the thread-local deadline for a bounded operation and return the previous value to
// restore. `millis <= 0` clears it. This is the seam tls.c reaches through: an mbedTLS
// read or handshake drives its socket waits through neon_net_wait, so bracketing the
// mbedTLS call in begin/end bounds every one of them by the same deadline a plain read
// would get. Defined in internal.h.
int64_t neon_net_deadline_begin(int64_t millis) {
    int64_t saved = t_net_deadline;
    t_net_deadline = millis <= 0 ? 0 : neon_net_now_ms() + millis;
    return saved;
}
void neon_net_deadline_end(int64_t saved) {
    t_net_deadline = saved;
}

// Wait until `fd` is readable (or writable), without stopping this OS thread if we are on a
// fiber. The hook is armed by the scheduler for the life of a runtime; off it, a plain poll.
// Not static: this is the seam `src/tls.c`'s BIO callbacks block through (declared in
// internal.h), so an mbedTLS handshake parks a fiber exactly the way a plain read does.
//
// Returns 0 ready, -errno on error, and -ETIMEDOUT when a deadline is armed and passes
// first — a value the read/connect loops turn into the timeout each is bounded by.
int neon_net_wait(int fd, bool for_write) {
    int64_t deadline = t_net_deadline;
    if (deadline != 0) {
        if (neon_fiber_readiness_deadline_hook != NULL) {
            int r = neon_fiber_readiness_deadline_hook(fd, for_write, deadline);
            if (r != 1) {
                return r; // 0 ready, -ETIMEDOUT timeout — the root falls through (r == 1)
            }
        }
        int64_t now = neon_net_now_ms();
        int ms = deadline <= now ? 0 : (int)(deadline - now > 2147483647 ? 2147483647
                                                                         : deadline - now);
        struct pollfd p = {fd, for_write ? POLLOUT : POLLIN, 0};
        int r;
        do {
            r = poll(&p, 1, ms);
        } while (r < 0 && errno == EINTR);
        if (r < 0) {
            return -errno;
        }
        return r == 0 ? -ETIMEDOUT : 0;
    }
    if (neon_fiber_readiness_hook != NULL) {
        // 0 = the fiber was parked and is now ready; negative = an error to report;
        // POSITIVE = "not mine" (the caller is the root context, not a fiber), which falls
        // through to the poll below. That third case is why this is not a plain tail call:
        // treating "not handled" as "ready" would retry the syscall, get EAGAIN, and spin.
        int r = neon_fiber_readiness_hook(fd, for_write);
        if (r <= 0) {
            return r;
        }
    }
    struct pollfd p;
    p.fd = fd;
    p.events = for_write ? POLLOUT : POLLIN;
    p.revents = 0;
    int r;
    do {
        r = poll(&p, 1, -1);
    } while (r < 0 && errno == EINTR);
    return r < 0 ? -errno : 0;
}

// ---- addresses ----
//
// An address is text, `host:port`, with an IPv6 host in brackets (`[::1]:8080`) so the
// colon that separates the port is unambiguous. Resolution is `getaddrinfo`, so a name
// works wherever a literal does.

// Split `addr` into NUL-terminated host and service. Returns false on a malformed address.
static bool neon_net_split(neon_str addr, char* host, size_t hostcap, char* serv,
                           size_t servcap) {
    size_t n = neon_str_len(&addr);
    const char* s = neon_str_data(&addr);
    if (n == 0 || n >= hostcap) {
        return false;
    }
    size_t colon;
    if (s[0] == '[') {
        const char* close = memchr(s, ']', n);
        if (close == NULL || (size_t)(close - s) + 1 >= n || close[1] != ':') {
            return false;
        }
        size_t hlen = (size_t)(close - s) - 1;
        memcpy(host, s + 1, hlen);
        host[hlen] = '\0';
        colon = (size_t)(close - s) + 1;
    } else {
        const char* last = NULL;
        for (size_t i = 0; i < n; i++) {
            if (s[i] == ':') {
                last = s + i;
            }
        }
        if (last == NULL) {
            return false;
        }
        size_t hlen = (size_t)(last - s);
        memcpy(host, s, hlen);
        host[hlen] = '\0';
        colon = hlen;
    }
    size_t slen = n - colon - 1;
    if (slen == 0 || slen >= servcap) {
        return false;
    }
    memcpy(serv, s + colon + 1, slen);
    serv[slen] = '\0';
    return true;
}

// Render a sockaddr back to `host:port` text, bracketing an IPv6 host.
static neon_str neon_net_render(const struct sockaddr* sa, socklen_t len) {
    char host[NI_MAXHOST];
    char serv[NI_MAXSERV];
    if (getnameinfo(sa, len, host, sizeof(host), serv, sizeof(serv),
                    NI_NUMERICHOST | NI_NUMERICSERV) != 0) {
        return neon_str_lit("", 0);
    }
    char out[NI_MAXHOST + NI_MAXSERV + 4];
    int n;
    if (sa->sa_family == AF_INET6) {
        n = snprintf(out, sizeof(out), "[%s]:%s", host, serv);
    } else {
        n = snprintf(out, sizeof(out), "%s:%s", host, serv);
    }
    if (n < 0) {
        return neon_str_lit("", 0);
    }
    return neon_str_new(out, (size_t)n);
}

static int neon_net_nonblock(int fd) {
    int fl = fcntl(fd, F_GETFL, 0);
    if (fl < 0) {
        return -errno;
    }
    if (fcntl(fd, F_SETFL, fl | O_NONBLOCK) < 0) {
        return -errno;
    }
    return 0;
}

// Resolve and hand each candidate to `use_it` until one succeeds. `passive` asks for a
// bindable address when the host is empty.
static int64_t neon_net_each(neon_str addr, int socktype, bool passive,
                             int64_t (*use_it)(int fd, const struct addrinfo* ai)) {
    char host[NI_MAXHOST];
    char serv[NI_MAXSERV];
    if (!neon_net_split(addr, host, sizeof(host), serv, sizeof(serv))) {
        neon_str_release(addr);
        return -EINVAL;
    }
    neon_str_release(addr);

    struct addrinfo hints;
    memset(&hints, 0, sizeof(hints));
    hints.ai_family = AF_UNSPEC;
    hints.ai_socktype = socktype;
    hints.ai_flags = passive ? AI_PASSIVE : 0;
    struct addrinfo* list = NULL;
    // An empty host means "any interface" for a listener, which getaddrinfo spells NULL.
    int rc = getaddrinfo(host[0] == '\0' ? NULL : host, serv, &hints, &list);
    if (rc != 0) {
        // A resolver failure is not an errno; EAI_SYSTEM aside, report it as EINVAL rather
        // than inventing a code. The message the stdlib prints names the address.
        return rc == EAI_SYSTEM ? -errno : -EINVAL;
    }

    int64_t last = -EADDRNOTAVAIL;
    for (struct addrinfo* ai = list; ai != NULL; ai = ai->ai_next) {
        int fd = socket(ai->ai_family, ai->ai_socktype | SOCK_CLOEXEC, ai->ai_protocol);
        if (fd < 0) {
            last = -errno;
            continue;
        }
        int nb = neon_net_nonblock(fd);
        if (nb < 0) {
            close(fd);
            last = nb;
            continue;
        }
        int64_t r = use_it(fd, ai);
        if (r >= 0) {
            freeaddrinfo(list);
            return r;
        }
        close(fd);
        last = r;
    }
    freeaddrinfo(list);
    return last;
}

// ---- TCP ----

static int64_t neon_net_do_listen(int fd, const struct addrinfo* ai) {
    int on = 1;
    // Without SO_REUSEADDR a listener cannot rebind while its old connections sit in
    // TIME_WAIT — the restart-my-server-immediately case, which is every server.
    setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &on, sizeof(on));
    if (bind(fd, ai->ai_addr, ai->ai_addrlen) < 0) {
        return -errno;
    }
    if (listen(fd, 128) < 0) {
        return -errno;
    }
    return fd;
}

int64_t neon_net_listen(neon_str addr, int64_t backlog) {
    (void)backlog; // the queue depth is fixed at 128; see neon_net_do_listen
    return neon_net_each(addr, SOCK_STREAM, true, neon_net_do_listen);
}

int64_t neon_net_accept(int64_t lfd, neon_str* peer) {
    for (;;) {
        struct sockaddr_storage ss;
        socklen_t slen = sizeof(ss);
        int fd = accept((int)lfd, (struct sockaddr*)&ss, &slen);
        if (fd >= 0) {
            int nb = neon_net_nonblock(fd);
            if (nb < 0) {
                close(fd);
                *peer = neon_str_lit("", 0);
                return nb;
            }
            *peer = neon_net_render((struct sockaddr*)&ss, slen);
            return fd;
        }
        if (errno == EINTR || errno == ECONNABORTED) {
            continue; // a connection died in the queue; the next one is unaffected
        }
        if (errno != EAGAIN && errno != EWOULDBLOCK) {
            *peer = neon_str_lit("", 0);
            return -errno;
        }
        int w = neon_net_wait((int)lfd, false); // park until a connection is queued
        if (w < 0) {
            *peer = neon_str_lit("", 0);
            return w;
        }
    }
}

static int64_t neon_net_do_connect(int fd, const struct addrinfo* ai) {
    for (;;) {
        if (connect(fd, ai->ai_addr, ai->ai_addrlen) == 0) {
            return fd;
        }
        if (errno == EINTR) {
            continue;
        }
        if (errno != EINPROGRESS && errno != EALREADY) {
            return -errno;
        }
        // A non-blocking connect completes asynchronously: wait for writability, then read
        // SO_ERROR — the connection's real verdict, which the writability alone does not
        // give (a refused connection also becomes writable).
        int w = neon_net_wait(fd, true);
        if (w < 0) {
            return w;
        }
        int soerr = 0;
        socklen_t elen = sizeof(soerr);
        if (getsockopt(fd, SOL_SOCKET, SO_ERROR, &soerr, &elen) < 0) {
            return -errno;
        }
        if (soerr != 0) {
            return -soerr;
        }
        return fd;
    }
}

int64_t neon_net_connect(neon_str addr) {
    return neon_net_each(addr, SOCK_STREAM, false, neon_net_do_connect);
}

// Connect, bounded by `millis` (<= 0 means no bound): a peer that accepts nothing — or a
// route that black-holes the SYN — no longer parks the fiber forever. -ETIMEDOUT on the
// deadline, which the stdlib turns into a TimeoutError. The thread-local deadline bounds
// every readiness wait `neon_net_each` makes across the address candidates.
int64_t neon_net_connect_timeout(neon_str addr, int64_t millis) {
    if (millis <= 0) {
        return neon_net_connect(addr);
    }
    t_net_deadline = neon_net_now_ms() + millis;
    int64_t r = neon_net_connect(addr);
    t_net_deadline = 0;
    return r;
}

// Read up to `max` bytes. A zero-length result means the peer closed (EOF) — the caller
// distinguishes it from "nothing yet", which cannot happen here because we waited.
neon_str neon_net_read(int64_t fd, int64_t max, int64_t* err) {
    *err = 0;
    if (max <= 0) {
        return neon_str_lit("", 0);
    }
    char* buf = (char*)malloc((size_t)max);
    if (buf == NULL) {
        neon_trap("out of memory");
    }
    for (;;) {
        neon_ssize got = recv((int)fd, buf, (size_t)max, 0);
        if (got >= 0) {
            neon_str s = neon_str_new(buf, (size_t)got);
            free(buf);
            return s;
        }
        if (errno == EINTR) {
            continue;
        }
        if (errno != EAGAIN && errno != EWOULDBLOCK) {
            *err = -(int64_t)errno;
            free(buf);
            return neon_str_lit("", 0);
        }
        int w = neon_net_wait((int)fd, false);
        if (w < 0) {
            *err = w;
            free(buf);
            return neon_str_lit("", 0);
        }
    }
}

// Read, bounded by `millis` (<= 0 means no bound): -ETIMEDOUT in `*err` when nothing
// arrives in time, so a client cannot hang on a peer that accepted the connection and then
// went silent. The bound is per-call — each `read_timeout` gets the full `millis` — which
// is what a request-level timeout wants when it reissues around a keep-alive loop.
neon_str neon_net_read_timeout(int64_t fd, int64_t max, int64_t millis, int64_t* err) {
    if (millis <= 0) {
        return neon_net_read(fd, max, err);
    }
    t_net_deadline = neon_net_now_ms() + millis;
    neon_str s = neon_net_read(fd, max, err);
    t_net_deadline = 0;
    return s;
}

// Write every byte of every part, waiting whenever the socket buffer fills. A short write
// is not an error on a stream socket, it is the normal case under backpressure, so this
// loops until the whole iolist is out.
int64_t neon_net_write(int64_t fd, neon_list* parts) {
    const neon_str* items = (const neon_str*)parts->data;
    int64_t status = 0;
    for (size_t i = 0; i < parts->len && status == 0; i++) {
        const char* p = neon_str_data(&items[i]);
        size_t left = neon_str_len(&items[i]);
        while (left > 0) {
            // MSG_NOSIGNAL: a write to a peer that has gone away must be an EPIPE we can
            // report, not a SIGPIPE that kills the process.
            neon_ssize n = send((int)fd, p, left, MSG_NOSIGNAL);
            if (n > 0) {
                p += n;
                left -= (size_t)n;
                continue;
            }
            if (n < 0 && errno == EINTR) {
                continue;
            }
            if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
                int w = neon_net_wait((int)fd, true);
                if (w < 0) {
                    status = w;
                    break;
                }
                continue;
            }
            status = n < 0 ? -(int64_t)errno : -EIO;
            break;
        }
    }
    neon_release((neon_header*)parts);
    return status;
}

int64_t neon_net_close(int64_t fd) {
    if (close((int)fd) < 0) {
        return -(int64_t)errno;
    }
    return 0;
}

// 0 = further reads, 1 = further writes, 2 = both. Half-close is what lets a client say
// "I am done sending" while still reading the reply — the shape every request/response
// protocol over a stream needs.
int64_t neon_net_shutdown(int64_t fd, int64_t how) {
    int h = how == 0 ? SHUT_RD : (how == 1 ? SHUT_WR : SHUT_RDWR);
    if (shutdown((int)fd, h) < 0) {
        return -(int64_t)errno;
    }
    return 0;
}

// The address this socket is actually bound to — the point of binding port 0 and asking.
neon_str neon_net_local_addr(int64_t fd, int64_t* err) {
    struct sockaddr_storage ss;
    socklen_t slen = sizeof(ss);
    if (getsockname((int)fd, (struct sockaddr*)&ss, &slen) < 0) {
        *err = -(int64_t)errno;
        return neon_str_lit("", 0);
    }
    *err = 0;
    return neon_net_render((struct sockaddr*)&ss, slen);
}

// Nagle off: send small writes immediately instead of coalescing them. What a
// request/response protocol wants, and what a bulk transfer does not.
int64_t neon_net_set_nodelay(int64_t fd, bool on) {
    int v = on ? 1 : 0;
    if (setsockopt((int)fd, IPPROTO_TCP, TCP_NODELAY, &v, sizeof(v)) < 0) {
        return -(int64_t)errno;
    }
    return 0;
}

// ---- UDP ----

static int64_t neon_net_do_bind(int fd, const struct addrinfo* ai) {
    int on = 1;
    setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &on, sizeof(on));
    if (bind(fd, ai->ai_addr, ai->ai_addrlen) < 0) {
        return -errno;
    }
    return fd;
}

int64_t neon_net_udp_bind(neon_str addr) {
    return neon_net_each(addr, SOCK_DGRAM, true, neon_net_do_bind);
}

int64_t neon_net_udp_send_to(int64_t fd, neon_str addr, neon_str data) {
    char host[NI_MAXHOST];
    char serv[NI_MAXSERV];
    if (!neon_net_split(addr, host, sizeof(host), serv, sizeof(serv))) {
        neon_str_release(addr);
        neon_str_release(data);
        return -EINVAL;
    }
    neon_str_release(addr);

    struct addrinfo hints;
    memset(&hints, 0, sizeof(hints));
    hints.ai_family = AF_UNSPEC;
    hints.ai_socktype = SOCK_DGRAM;
    struct addrinfo* list = NULL;
    if (getaddrinfo(host, serv, &hints, &list) != 0) {
        neon_str_release(data);
        return -EINVAL;
    }

    int64_t status = -EADDRNOTAVAIL;
    for (struct addrinfo* ai = list; ai != NULL; ai = ai->ai_next) {
        for (;;) {
            neon_ssize n = sendto((int)fd, neon_str_data(&data), neon_str_len(&data),
                                  MSG_NOSIGNAL, ai->ai_addr, ai->ai_addrlen);
            if (n >= 0) {
                status = n; // a datagram goes whole or not at all: no partial-send loop
                break;
            }
            if (errno == EINTR) {
                continue;
            }
            if (errno == EAGAIN || errno == EWOULDBLOCK) {
                int w = neon_net_wait((int)fd, true);
                if (w < 0) {
                    status = w;
                    break;
                }
                continue;
            }
            status = -(int64_t)errno;
            break;
        }
        if (status >= 0) {
            break;
        }
    }
    freeaddrinfo(list);
    neon_str_release(data);
    return status;
}

// One datagram, with the sender's address. A datagram longer than `max` is TRUNCATED and
// the rest discarded — that is the protocol's semantics, not a bug to loop around.
neon_str neon_net_udp_recv_from(int64_t fd, int64_t max, neon_str* peer, int64_t* err) {
    *err = 0;
    *peer = neon_str_lit("", 0);
    if (max <= 0) {
        return neon_str_lit("", 0);
    }
    char* buf = (char*)malloc((size_t)max);
    if (buf == NULL) {
        neon_trap("out of memory");
    }
    for (;;) {
        struct sockaddr_storage ss;
        socklen_t slen = sizeof(ss);
        neon_ssize got =
            recvfrom((int)fd, buf, (size_t)max, 0, (struct sockaddr*)&ss, &slen);
        if (got >= 0) {
            *peer = neon_net_render((struct sockaddr*)&ss, slen);
            neon_str s = neon_str_new(buf, (size_t)got);
            free(buf);
            return s;
        }
        if (errno == EINTR) {
            continue;
        }
        if (errno != EAGAIN && errno != EWOULDBLOCK) {
            *err = -(int64_t)errno;
            free(buf);
            return neon_str_lit("", 0);
        }
        int w = neon_net_wait((int)fd, false);
        if (w < 0) {
            *err = w;
            free(buf);
            return neon_str_lit("", 0);
        }
    }
}

#endif
