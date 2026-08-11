#ifndef NEON_NET_H
#define NEON_NET_H

// Sockets — the primitives behind `std::net`. See `src/net.c` for the design; the shape of
// this API follows the rest of the runtime's IO: an `int64_t` return is a descriptor or a
// negative errno, and a native that produces data *and* can fail uses out-parameters, which
// codegen hands back to Neon as a tuple.
//
// Every descriptor here is non-blocking, and an operation that would block waits for
// readiness first — parking the calling FIBER if there is one, polling otherwise. So these
// are the same functions on a fiber and on `main`, which is the whole reason the surface
// needs no `async` colour.

#include <stdbool.h>
#include <stdint.h>

#include "neon/core.h"
#include "neon/list.h"

// ---- TCP ----

// Bind and listen. `addr` is `host:port` (`[::1]:8080` for a literal IPv6 host); an empty
// host means every interface. Returns the listening descriptor or a negative errno.
int64_t neon_net_listen(neon_str addr, int64_t backlog);

// Accept one connection, waiting until one arrives. Returns the connected descriptor (or a
// negative errno) and writes the peer's address.
int64_t neon_net_accept(int64_t fd, neon_str* peer);

// Connect, waiting for the handshake to finish. Returns the descriptor or a negative errno.
int64_t neon_net_connect(neon_str addr);
int64_t neon_net_connect_timeout(neon_str addr, int64_t millis);
bool neon_net_is_timeout(int64_t code);

// Up to `max` bytes, waiting until some arrive. An empty result with `*err == 0` is EOF:
// the peer closed its end.
neon_str neon_net_read(int64_t fd, int64_t max, int64_t* err);
neon_str neon_net_read_timeout(int64_t fd, int64_t max, int64_t millis, int64_t* err);

// Every byte of every part, waiting whenever the socket buffer fills. Consumes `parts`.
int64_t neon_net_write(int64_t fd, neon_list* parts);

int64_t neon_net_close(int64_t fd);

// 0 = further reads, 1 = further writes, 2 = both.
int64_t neon_net_shutdown(int64_t fd, int64_t how);

// The address this socket is bound to — how a listener on port 0 learns its port.
neon_str neon_net_local_addr(int64_t fd, int64_t* err);

// Nagle off: send small writes immediately rather than coalescing them.
int64_t neon_net_set_nodelay(int64_t fd, bool on);

// ---- UDP ----

int64_t neon_net_udp_bind(neon_str addr);

// One datagram to `addr`. Returns the bytes sent or a negative errno. Consumes both strings.
int64_t neon_net_udp_send_to(int64_t fd, neon_str addr, neon_str data);

// One datagram, with the sender's address. A datagram longer than `max` is truncated.
neon_str neon_net_udp_recv_from(int64_t fd, int64_t max, neon_str* peer, int64_t* err);

#endif
