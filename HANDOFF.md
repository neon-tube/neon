# Neon — handoff

Repo: `/home/kibb/projects/neon-tube/neon` (Rust → C compiler + C runtime), branch `main`.

## Standing rules

- **No `Co-Authored-By` trailers** in any commit, in any neon-tube repo.
- **No `mut`** — the language has no such keyword. `let` rebinds; the optimiser earns mutation.
- **No `-> null`** — never explicitly return null, and null must not be the implicit return
  type. Arrowless `throws E` and `-> ()` are unit. The checker now enforces it: `null`
  against a `()` return is an error.
- **`neon_retain`/`neon_release` admit no additions.** Measured 26–44% regressions for every
  attempted branch on that pair. Any design that wants a branch there is wrong; find another
  seam. This is why fiber isolation uses non-atomic `rc`, resource ownership costs zero
  atomics, and handles are two objects.
- Everything is driven from `cargo make` (see `docs/design/toolchain.md`). After one
  `cargo make rt`, plain `cargo build`/`cargo test` work for Rust-only loops; the runtime
  crate's build script only *locates* the prebuilt C archives and refuses stale ones.

## State: green

- `cargo make test` — full corpus (330 trials), 599 unit, all cli/ suites.
- `cargo make rt-tests` — 211 C tests under **both** `NEON_IO=io_uring` and `NEON_IO=epoll`.

## What is built (the arc of this work)

The concurrency + networking stack, on the isolated-arena fiber runtime:

1. **Resource ownership + `move`** — a `Resource` is an owned identity (shared body, per-holder
   refs, one owning ref = the baton). The baton moves at three visible edges (`move` lambda,
   `channel::send`, task return); misuse is a named catchable error (`NotOwnerError`,
   `ReleasedError`, `ClosedError`). `docs/design/fibers.md` §"Resources: owned identities".
2. **Toolchain** — `Makefile.toml` is the front door; cmake and cargo are peers cargo-make
   sequences; `runtime/build.rs` is a stale-refusing locator. `docs/design/toolchain.md`.
3. **`std::http`** — client + server (split into `http::client`/`http::server`), HTTP/1.1,
   keep-alive, chunked decode, a pipe-threaded request builder. Parses via **llhttp**.
4. **`std::url`** — RFC 3986 via **uriparser**, raw (still-encoded) components + `decode`.
5. **`net::tls`** — **mbedTLS 3.6.7 LTS**, client and server, on the fiber BIO seam (mbedTLS
   in blocking mode over `neon_net_wait`; no WANT_READ/WRITE). Verify-by-default with a loud
   `connect_insecure`, OS trust store at runtime. `https://` in `http::client`.
6. **Network timeouts** — `neon_fiber_io_wait_deadline` (epoll oneshot + a sleeper, loser
   cancelled); a thread-local net deadline `neon_net_wait` consults (covers TLS free);
   `tcp`/`tls` `connect_timeout`/`read_timeout`, `net::TimeoutError`, `http::client` `timeout`.
7. **`json::parse`** — the module had no text parser; added recursive-descent parse (escapes,
   `\uXXXX`, surrogate pairs), plus `client::json`/`server::json_response` glue.
8. **`channel::select_recv`** — receive over N channels, atomic single-winner claim,
   body-address lock order. Adversarially reviewed clean.
9. **`std::cancel`** — a token is a channel that only closes: `cancel`/`is_cancelled`/`check`,
   `channel` (to `select` on for wait-interruption), and `scope` (auto-cancel on region exit,
   throw-safe via a resource guard).
10. **`task::recover`** — supervise a crash: an awaited task's trap surfaces as a distinct
    non-`Error` `Panic`, not folded into `throws`. Longjmp-free (the crashed work's own fiber
    was torn down by crash isolation; recover reads the `failed` flag). `await` stays the
    propagating default.

Three checker papercuts also fixed: `is T as x` inline binder; `null` ≠ `()`; empty block
`{ }` = unit. And a real **codegen miscompile** fixed properly: joining same-arity tuples
where one element is a supertype of the other (`(str,i64) | (J,i64)`) now absorbs to the
element-wise join instead of a mis-read tagged union (was a silent miscompile; the inert-diff
confirmed no other program moved).

## Two delegated pieces were adversarially reviewed

Select and the tuple-join fix were built by subagents on pre-fibers bases, merged carefully,
and re-validated against the *whole* integrated suite (which is how two `null`-straggler
regressions surfaced). Both got a focused adversarial review: select's found one real
low-severity liveness edge (a sender crashing mid-copy forecloses the whole select — the
correct trade for exactly-once, documented); the tuple-join fix chose absorption over
element-wise widening (which would invent `(str,str)` and break exhaustiveness) and the
inert-diff (1975 observables) showed every pre-existing program byte-identical.

## Open, tracked honestly (nothing faked)

1. **Structured-concurrency `scope` with join** — the primitives are all here now
   (`task::recover` for the failure, the cancel token for the signal). A `scope` that spawns
   children, recovers each, and cancels the siblings on the first `Panic` is a stdlib
   combinator away — the highest-leverage next piece.
2. **`Panic` cause capture** — recover surfaces *that* a task crashed, not *why*. Threading
   the trap message from `neon_trap` through the crash path into the task is contained.
3. **`cancel::child`** — a parent→child context tree; needs a bounded-lifetime watcher fiber
   so a never-cancelled parent cannot leave it blocked and stop the runtime draining.
4. **Lambda-param inference through an unsolved generic** — `apply[R](f: (i64) -> R)` needs
   `(n: i64)` written. A known limitation, root-caused and documented in
   `docs/design/typechecker.md` §"Known limitation"; the sound fix (variance-aware
   substitution of unsolved generics) is real work in the inference core, not worth
   destabilising for a one-token annotation. Two attempts were tried and reverted.
5. **Send-select / heterogeneous select**, `select_recv_timeout` — receive-select is built;
   these are noted in `docs/design/fibers.md`'s not-built list with their shapes.

## CBMC models

`runtime/models/` has the resource body/ref + baton models (mutation-validated) and three
scheduler-invariant models. The `select` and timeout machinery are not modelled.
