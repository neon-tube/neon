# http-datetime

A static HTTP server, in each language, that answers every request with the current
time in ISO 8601. It exercises the whole path a real service pays for on every hit:
accept, parse the request, read the clock, format it, and write the response.

## The contract every implementation meets

- Listen on `127.0.0.1:$PORT` (TCP), where `PORT` defaults to `18080`.
- For any HTTP request, reply `200 OK`, `Content-Type: text/plain`, body = the current
  UTC time in ISO 8601 (`2026-08-11T17:30:00Z`; sub-second precision optional).
- Compute the time PER REQUEST (the point of the benchmark).
- Support HTTP/1.1 **keep-alive** (persistent connections). The load generator reuses
  connections, so a server that closes after every response is measuring TCP setup, not
  itself. Servers built on a standard-library HTTP stack get this for free; the raw-socket
  implementations loop, reading requests on the same connection until the peer hangs up.
- Standard toolchain only, no third-party packages. Languages without an HTTP server in
  their standard library implement a minimal HTTP/1.1 handler over a raw socket.

## Running

The real numbers come from [`rewrk`](https://github.com/lnx-search/rewrk), a keep-alive
HTTP load generator, with a sensible connection/thread count:

    rewrk -c 64 -t 8 -d 10s -h http://127.0.0.1:18080

`run_bench.py` automates that: it builds every available language, starts each server on a
free port, checks it returns a parseable ISO timestamp, runs `rewrk` against it, and prints
a requests/second table.

    python3 run_bench.py                 # every available language
    python3 run_bench.py --only neon,go  # a subset
    python3 run_bench.py -c 128 -t 12 -d 15s
