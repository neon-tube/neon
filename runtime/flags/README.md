# Runtime C flags — one authority

One flag per line, GNU spellings. Read by:

- `runtime/CMakeLists.txt` (`file(STRINGS ...)`) — the archive variants and the test
  binary's base flags. The MSVC spellings stay in the CMakeLists' MSVC branch; this
  directory is the GNU family only.
- `Makefile.toml`'s `smoke` task — the portability smoke compile line.
- `cli/tests` — a mechanical-agreement test asserts `buildcfg::SANITIZED_VARIANT_COVERS`
  matches `san.txt`, so the CLI's refusal logic cannot drift from what the archive was
  actually instrumented with.

Probed flags (`-flto -ffat-lto-objects`, `-march=native`) are appended by CMake after its
probes and deliberately do NOT live here: a file of flags must never claim something the
compiler was not asked to prove.
