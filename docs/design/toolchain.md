# The toolchain: cargo-make owns the sequence

Status: **built** (2026-08-10). One ruling, from the user: people drive the build from the
top-level make, so the orchestrator is load-bearing, not a convenience — cmake and cargo
are PEERS that never invoke each other, and cargo-make sequences them.

## The shape

```
cargo make rt        tools/rt.sh: cmake builds the archive variants per compiler family
                     → target/neon-rt/<flavor>/{lib,include} + .stamp
cargo make build     rt → cargo build → tools/bundle.sh: sysroot staged at target/<profile>
cargo make release   the same, optimized
cargo make test      rt → release build → bundle → cargo test (corpus links the archives)
cargo make rt-tests  cmake+ctest: the C unit suite (ASan/UBSan), both IO engines
cargo make models    CBMC verify-all
cargo make smoke     portability smoke, flags from runtime/flags/
cargo make dist      tools/dist.sh: an installable prefix (one layout definition,
                     shared by the release workflow and the Dockerfile)
```

`runtime/build.rs` is a **locator**: it verifies the archives exist and are *newer than
the C sources* and publishes their path; stale or missing is a refusal that names
`cargo make rt` — never a silent link of yesterday's runtime, and never an invocation of
anything. That is what makes `cargo check`/rust-analyzer instant (the old build script
configured and built two cmake trees on every rerun) and the Rust-only inner loop pure
cargo after one `rt`.

`cli/build.rs` is **gone**: staging the sysroot was a build script writing outside its
own OUT_DIR into `target/<profile>/` — a documented cargo anti-pattern with a real race
against rust-analyzer — and sequencing belongs to the orchestrator. `tools/bundle.sh` does
it as an explicit step.

## What this fixed (see the investigation that preceded it)

- The same C sources were compiled by five drivers with five flag regimes; the GNU flag
  sets now live in `runtime/flags/*.txt`, read by CMake, the smoke task, and asserted
  against the CLI's `SANITIZED_VARIANT_COVERS` by a unit test. Probed flags (LTO,
  `-march=native`) stay in CMake, after their probes.
- Stale-flavor holes: a compiler removed from PATH left its old archives linkable
  forever; `tools/rt.sh` deletes the flavor it can no longer build, and the locator's
  freshness check covers the rest.
- `cargo build` was non-hermetic: network one `-D` away, artifact set dependent on PATH,
  cmake invoked per `cargo check`. The cargo path now touches no tool at all.

## Deliberate edges

- Bare `cargo build` on a fresh clone fails until `cargo make rt` has run — with an error
  naming the command. That is the peers model's price, accepted explicitly.
- The Windows CI job keeps its hand-written cmake steps: cargo-make inside msys2 shells
  buys nothing there, and the MSVC flag spellings stay in the CMakeLists' MSVC branch.
- The Dockerfile and install.sh call `tools/*.sh` directly rather than bootstrapping
  cargo-make — same sequence, same scripts, no extra dependency in images/installers.
- FetchContent (tinyunit, llhttp) lives only in the test-gated CMake path, so the cargo
  path stays offline. If runtime C ever calls llhttp, that becomes a vendoring decision
  to take deliberately (the investigation's note stands).
