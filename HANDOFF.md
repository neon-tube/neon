# Neon — handoff

Repo: `/home/kibb/projects/neon-tube/neon` (Rust → C compiler + C runtime), branch `main`.

## Standing rules

- **No `Co-Authored-By` trailers** in any commit, in any neon-tube repo.
- **No `mut`** — the language has no such keyword. `let` rebinds; the optimiser earns mutation.
- **No `-> null`** (ruled 2026-08-10): never explicitly return null, and null must not be
  the implicit return type. The stdlib+corpus sweep LANDED in 6649595 (arrowless
  `throws E` / `-> ()` are the spellings); do not add new violations. Checker-side
  enforcement gaps remain — see Tracked next tasks.
- `cargo fmt` is fine to run; the repo is rustfmt-clean.
- **`neon_retain`/`neon_release` admit no additions.** Measured 26–44% regressions for every
  attempted branch. The resource ownership design (below) was shaped by this: batons cost
  zero atomics; the body's concurrent count is touched only at ref birth/death.

## State: resource ownership SHIPPED (commit db981ba)

Resources are **owned identities**. Design and mechanics: `docs/design/fibers.md`
§"Resources: owned identities and the baton" (current, built-state). The one-paragraph
version: body/ref split (the channel pattern), exactly one OWNING ref whose death runs
cleanup, the baton moves at three visible edges — `move () => ...` (new closure-head
keyword), `channel::send` (throw ⇒ not sent), task return — and every misuse is a named
catchable error (`NotOwnerError`, `ReleasedError`, `ClosedError`; the send/close traps are
gone). `tcp::serve` is an ordinary loop; the raw-fd laundering is deleted. Compile errors:
plain resource capture at a spawn/runtime site ("needs `move`"), `move` with nothing to
move. Fixed in passing: crashed-fiber env leak, task-await slab leak (await now restages),
`neon_channel_send`'s discarded result.

Green: 315 corpus trials (4 new fiber tests), 590 unit, 190 C tests under both
`NEON_IO=io_uring` and `NEON_IO=epoll`.

How it was decided, for the record: dup-on-copy and global sharing were considered and
rejected (silent write-interleaving; release races; nondeterministic cleanup). The model is
Erlang's controlling-process with the baton pass automated at the three edges. Deliberate
judgment calls a future session should not relitigate casually: non-owner `release`/`take`
are silent no-ops (double-release precedent; preserves `Resource[T, never]`'s no-`try`
release); `move` is closure-head, whole-closure, self-scoping (values copy regardless);
fiber-local ("pinned") resources are unshipped until a type needs one.

## In flight

- **CBMC resource models**: LANDED (c944a70) — the five `resource-*` models reworked for
  body/ref plus `a-transfer-leaves-exactly-one-owning-ref` (916 props), all mutation-
  validated (42-run campaign; the gate, the demotion, the bracket condition, the claim and
  the env-release move each caught by name). verify-all: 37/37. One tractability note
  recorded in the model headers: the root sentinel does not constant-fold in symex, so
  cross-context scenarios stand on fiber-object identities.
- **llhttp**: MERGED (bcea3e0) — CMake FetchContent, pinned `release/v9.4.3` by sha256,
  gated behind NEON_RT_TESTS so `cargo build` never fetches; smoke test in the C suite.
- **Toolchain**: REDESIGNED AND LANDED (the user ruled for the PEERS model, orchestrator
  load-bearing). `docs/design/toolchain.md` is the record: cargo-make sequences cmake and
  cargo (`Makefile.toml` is the menu), `tools/{rt,bundle,dist}.sh` are the steps,
  `runtime/build.rs` is a stale-refusing locator, `cli/build.rs` is deleted, GNU flags
  live in `runtime/flags/` (CMake + smoke + a cli unit test read them), CI's Linux jobs
  call recipes, Dockerfile/install.sh call the scripts. Drive everything via
  `cargo make <task>`; after one `cargo make rt`, bare cargo works for Rust-only loops.

## Tracked next tasks

1. `std::http` SHIPPED (client + server, HTTP/1.1, keep-alive, chunked decode; llhttp is
   FetchContent-pinned into the archives — the `cargo make rt` step fetches, the cargo
   path stays offline). Next on that road, both researched and task-tracked: URL parsing
   via uriparser (replaces http::split_url) and TLS via mbedTLS on the fiber BIO seam
   (mbedtls_ssl_set_bio callbacks -> the existing fiber-parking socket ops).
2. Known checker gaps found while testing, worth fixing:
   - `expr is T as x` fails on nullable runtime records and on any `T | null` with a
     qualified generic (`is` on a BINDING works — see `resource_through_channel.neon`'s
     comment); the error message is the nonsensical "a `bool` can never be a `#error`".
   - null→unit coercion is inconsistent: an annotated lambda `(n: i64) => null` against
     `-> ()` is rejected, but a fn BODY ending in `null` against a unit return is
     silently accepted — the enforcement gap the null-as-unit ruling (swept in 6649595)
     still leaves open.
   - An EMPTY lambda block `{ }` types as the empty record `{}`, not unit, so a
     do-nothing cleanup needs a filler statement.

## Residual design notes (documented, deliberate)

- Composite mid-copy trap on a buffered send can orphan a staged owning ref → fd
  leak-on-crash. Requires a resource next to an unsendable sibling in one record. Known,
  narrow, documented in the design section.
- `is` narrowing gap above; `Resource` equality is per-ref (two refs to one body compare
  unequal) — same standing quirk channels have.
- `neon check` now runs lowering too (6649595), so the `move` diagnostics fire on check,
  build and run alike.
