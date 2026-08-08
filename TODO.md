# TODO

What is known-broken or undecided. Resolved items are **removed**, not struck through: their
reasoning lives in the design doc it produced (`docs/design/`) and their behaviour in the
corpus file that pins it. The exception is the perf sections, where a completed item is the
measurement that explains the current number and deleting it would make the remaining items
unreadable.

The P0 and P1 sections are empty. They were emptied by the 2026-07-22 audit sweep, refilled
by the serialization and narrowing work of 2026-08-03/04 — six wrong-answer defects, every
one a program that compiled and did the wrong thing — and emptied again. The last of them
was found on 2026-08-08 by re-verifying a design doc's claim rather than by anyone hitting
it: a protocol's default body printed a compiler marker as program output. Each fix is
corpus-pinned.

What is left is of four kinds:

- **A design worth having**: `TODO` erroring at the call site rather than the definition.
- **Owner's calls**: block comments (§16), Kani's timing (§18), and the stdlib-derive
  direction (§19).
- **Measured work**: the three perf profiles, `@cfg`'s host/target split (§17), and one
  verification-tooling gap (13c).
- **Unproven leads**, kept deliberately separate because nobody has built a repro. Three of
  the four anyone has investigated turned out to be real defects.

Each item still has a repro or a file:line.

---

## Design — `TODO` should error at the CALL site, not the definition

`TODO("why")` type-checks and panics if reached, as Rust's `todo!()` does. That is the
second design; the first made it a compile error where it sat, and the first real use argued
it down within a day.

`std::fs`'s Windows half is a wall of `TODO`s. Erroring at the DEFINITION took the whole
module out on Windows: a program calling only `fs::read` — implemented everywhere, touching
no hole — collected five errors about functions it never mentions. A marker meant to say
"this part is missing" was saying "this library is unusable".

The runtime form has its own cost, and it is the one Rust lives with: a `TODO` can ship, and
nothing in a signature warns the caller.

**Neither is the design worth having.** That is an error at the CALL site: `fs::read`
compiles and runs on Windows, `fs::mkdir(p)` is a compile error naming what is missing. It
gives the safety of the first form without the blast radius, and it is strictly better than
Rust's answer rather than a copy of it.

What it needs: the checker records a function whose body contains a `TODO` as holed, and
diagnoses calls to it with the TODO's message. Transitivity needs no special handling —
calling a holed function is itself an error, so a wrapper is caught at its own call. The
question worth deciding first is whether a `TODO` in ONE branch holes the whole function.
Conservative (yes) is sound and refuses code that would have run; precise (no) needs
reachability nobody has. Conservative first, and say so in the message.

---

## P2 — decisions. These need an owner's call, not an implementation.

### 16. Should block comments exist?

They nest, deliberately and correctly — commenting out a region containing `*/` must not
end early. But `//` plus an editor covers the use case, and dropping them removes the
tree-sitter external scanner entirely (nesting is why it exists).

---

## Perf — what the word-frequency profile says to build

From `bench/word-frequency/` (10M generated tokens counted in a `Map[str, i64]`),
profiled 2026-07-20. Neon: 0.67s, 1.69× C. The map is NOT the bottleneck —
`neon_map_slot` is ~12% — the strings around it are: ~40% of the run is
snprintf-family digit formatting inside `neon_i64_to_string`, and ~8.5% is `cfree`
releasing 10M temporary five-byte keys. Two languages beat C on this bench and each is
a tell: Zig at 0.51× formats integers with generated code (no snprintf), LuaJIT at
0.76× interns strings so table keys are nearly free.

**Status: 1.69× C → 0.90×.** Neon is faster than the C reference on this benchmark.
Items 1 and 2 are done; item 3 was **built and rejected** (see below); item 4 is unstarted
and its premise has changed. Re-profile before building anything here — every cost the
original profile named has now been paid off or disproved.

Item 3, small-string optimisation, is **done and not merged**: it works, and it is neutral
on word-frequency and 7.9% *worse* on brainfuck. `docs/design/small-strings.md` has the
result and the reasoning error behind it; the implementation is on `sso-experiment`. The
premise — "77% of the profile is `malloc`/`cfree`, so removing the allocation removes most
of the run" — confused profile share with recoverable time. Do not re-propose it from that
profile.

What the SSO attempt *did* find, and what has since landed instead: the expensive thing
about a five-byte string was never the allocation, it was **calling into libc to touch it**.
Short-length fast paths in `neon_str_eq`, `neon_str_cmp` and `neon_str_new` are worth
**-8.6% on word-frequency and -3.6% on brainfuck**, against a measured byte-loop/`memcmp`
crossover at 7 bytes (`NEON_STR_SHORT`). That is more than SSO offered, at a fraction of the
risk, and it needed no representation change at all.

In order — each item stands alone, and the first two are runtime-only:

1. ~~**Hand-rolled itoa in `neon_i64_to_string`**~~ — **done** (`runtime/src/string.c`,
   not `rt.c` as this entry originally said). Digit loop into a fixed 20-byte buffer, one
   copy out, negation through `uint64_t` so `INT64_MIN` is not UB. Worth **0.67s → 0.54s**,
   1.69× C down to **1.50×**. It did not cross C: the C reference pays `snprintf` too, but
   the remaining gap is the temp-key traffic that items 2 and 3 target, not formatting.
2. ~~**A real map upsert.**~~ — **done**, as `map::update`. This entry undercounted the
   cost: `set(m, k, get_or(m, k, 0) + 1)` is *three* passes, not two, because `get_or` is
   itself `contains` followed by an index. `map::update(m, k, fallback, f)` probes once.
   Worth **0.54s → 0.35s**, and it crosses C — **0.95×**, from 1.69× where this section
   started.

   Not named `upsert` in the end: `map::set` is already insert-or-update, so the word
   would not have distinguished them. What is new is that the value is computed *from*
   the old one, which is `update` (Clojure, Scala) rather than `upsert` (SQL).

   Deliberately a method rather than an IR fusion of the three-pass idiom. Fusing would
   have to prove both maps are the same value and that nothing observes it in between;
   worse, when the proof failed it would fail *silently*, leaving a 2× cliff with no
   diagnostic. The fast shape is one you ask for.

   The runtime cannot call a `(V) -> V` closure itself — that call's C signature depends
   on `V` — so codegen emits a `nmap_upd_*` shim per value repr, the same division of
   labour as `nres_drop_*` for a resource's cleanup. Measured and rejected: keying the
   shim on the closure's target too, so the inner call is direct and inlinable. It is
   worth 0.2% (0.572s → 0.571s on an identical build); GCC already speculatively
   devirtualizes, and the cost is hashing and allocation, not the indirect call.

   Still unbuilt from this entry, and still worth it: **borrow-key insertion** — probe
   with the caller's scratch key and copy into owned storage only on first insert. That
   is what deletes the temp-key frees, and `update` does not do it yet; it still consumes
   a freshly built key per token.
3. **Small-string optimisation in `neon_str`.** Every key here fits inline; SSO removes
   all per-token heap traffic and makes hashing/equality pointer-chase-free. Highest
   ceiling and it compounds everywhere — but it is a representation/ABI change across
   runtime, codegen and the witnesses. A project, not a patch; wants its own CBMC model
   before anything relies on it.
4. **Fuse interpolation into a sized concat-n.** `"w#{id}"` is `to_string` then
   `str_concat`: two allocations where one suffices, and `lower.rs:1505` already
   confesses the n-hole fold is quadratic. Small lowering + runtime change; modest on
   this bench, real for any multi-hole interpolation.

Explicitly declined for now: a map sequel to `ir::unique`'s in-place rewrite
(`neon_map_set_inplace`). The counts map is sole-owned round the loop — the shape
matches — but the per-write `rc` test it would remove is noise on this profile, since
`neon_map_set` already mutates in place at `rc == 1`. Wrong benchmark to justify it.

---

## Perf — what the binary-trees profile says to build

From `bench/binary-trees/` (67M short-lived recursive nodes, built, walked, dropped),
profiled 2026-07-20. Neon: 0.77s, 1.16× C — tied with C++, ahead of Go (1.40×), Rust
(1.49×) and Zig (1.85×). The refcounting is NOT the cost: ~60% of the run is glibc
allocator internals (`_int_malloc`/`_int_free` and friends), ~8% the generated Node
drop (`ned0`), ~20% the program's own make/check recursion — the same shape as C's own
profile. The languages that beat C (Java 0.51×, Bun 0.62×, C# 0.69×) all do it with a
generational nursery: pointer-bump allocation and never touching the dead.

In order:

1. **A size-class slab behind `neon_alloc`.** Small same-size objects from a free list:
   alloc is a pop, free is a push, versus glibc's bin machinery eating half the run.
   This is also FBIP reuse arriving by the runtime door — the loop interleaves dropping
   tree *i* with building tree *i+1*, so the slab recycles cache-hot slots exactly
   where a compiler reuse-token analysis cannot reach (alloc and free live in different
   functions here). Runtime-only. Projection: under C, since C stays on glibc. Costs:
   a sizing/fragmentation policy, and `runtime/models/` must learn the new heap.
   Bonus: also deletes word-frequency's per-token `cfree` cost.
2. **Devirtualise the drop.** Releases go through the header's function pointer — an
   indirect call per node, opaque to gcc. At a typed release site codegen knows the
   repr and can call the concrete drop directly (keeping the `rc == 0` test), letting
   small drops inline. A few percent, and it removes the same indirect-call barriers
   the brainfuck work just paid to remove elsewhere.
3. **Header diet.** The layout today is rc at +0, flags at +8, drop pointer at +16 — a
   24-byte header on a 16-byte payload, so a Node is 40 bytes to C's 16. Pack rc+flags
   and turn the drop pointer into a type index and the header halves; every heap object
   in every program shrinks with it. ABI change across runtime, codegen and the
   witnesses — a planned project with its own model updates, same tier as `neon_str`
   SSO above.
4. **The recursion itself: nothing.** make/check compile to the same shape and cost as
   C's. `ir::unique` has no purchase — nothing is a loop-carried list.

Non-options, on the record: a generational nursery is what actually wins this bench,
but deferred reclamation breaks eager deterministic destruction — a semantics change,
not an optimisation. Arena/region allocation needs region inference to be safe —
research, not backlog.

The two benchmark sections triangulate: both profiles put the cost in the allocator
and object layout, and neither puts it in the refcounting. That is the runtime's next
frontier.

---

## Perf — what the n-body profile says to build

From `bench/n-body/` (20M steps of the benchmarks-game integrator over a `List[Body]`,
`Body` a flat record of seven f64s), profiled 2026-07-20 *after* the nested-loop fix to
`ir::unique`'s order rule (which this benchmark flushed out; the fix took 4.45s →
2.83s). Neon: 2.83s, ~4.4× C — and 88% of the entire run is three `movups` stores: the
in-place record writes themselves. The rewrite did its job — stable pointer, no rc
traffic, stores go straight into the buffer — what remains is *granularity*.

**Partial record update.** The idiomatic write rebuilds the whole record:

    bodies = try! list::set(bodies, i, Body { vx: new_vx, vy: .., vz: .., x: bi.x, .. });

so every velocity update copies 56 bytes out (`bodies[i]`), rebuilds seven fields, and
stores 56 bytes back — where C stores the three changed doubles, 24 bytes, no copy-out.
4× the memory traffic per pair-interaction, plus store-forwarding stalls when the next
iteration reloads a just-stored record; that is the whole remaining gap.

The shape of the fix, as an extension of the sole-ownership rewrite (which already
proves the buffer is exclusively ours): when the stored value is a `MakeRecord` whose
unchanged fields are `Field` projections of the same list's same slot (`bodies[i]` with
a subset replaced), emit stores for only the replaced fields — a field-offset variant of
`neon_list_set_inplace`. Needs: the literal-matches-slot proof (the record read and the
write index must be the same value, not merely equal), a field-offset store primitive,
and the same-buffer argument extended to partial writes. Not an afternoon; design it
against the `advance` IR before building.

Until then the program-level workaround exists and is honest to note: structure-of-
arrays (seven `List[f64]`s) turns every write into an 8-byte scalar store and would
likely put this benchmark near C today — but the benchmark keeps the record shape on
purpose, because the record shape is what people write.

**Half of this is now built: `ir::partial`.** It matches the shape below — a
`neon_list_set_inplace` whose record's unchanged fields are `field` projections of an
`index` of the same list at the same index value — and emits one
`neon_list_set_field_inplace` per changed field instead of a whole-record store. n-body
goes **4.35x C to 2.09x**; the other three benchmarks are unmoved.

**Now the whole of it: n-body is at parity, 1.02x C.** The third write site is taken too.
`advance` writes slot `i` and then slot `j`, and the write to `j` reads `xs[j]` with the
write to `i` in between; the pass keeps it only if it can prove `i != j`. It can, because
`j` is a counter that enters the inner loop at `i + 1` and climbs by 1 while `i` stays
fixed. `ir::partial::climbs_away_from` is that proof. It is narrow on purpose -- stride
exactly 1, both loops guarded by `< L` against the same length, so the ordering cannot wrap
`i64` -- and declines anything it cannot walk end to end (a descending counter, a wider
stride, a non-constant bound). The overflow reasoning, which no test can reach, is proved in
`verify/src/distinct.rs`.

**Measured ceiling, and four things not to try.** The fix above was re-derived
independently on 2026-07-20 and, this time, its value was measured rather than argued.
Patching the *generated* C to store only the changed fields — same program, identical
output to nine decimals:

| variant | time | x C |
|---|---|---|
| today | 2.82s | 4.41x |
| partial stores at the 2 provably-safe sites | 1.62s | 2.53x |
| partial stores at all 3 sites | 0.92s | **1.43x** |

So the item is worth 4.41x -> 1.43x, and a *conservative* version that declines the hard
site still gets 4.41x -> 2.53x. That matters for scoping: two of the three writes in
`advance` need no aliasing argument at all (the record is read from the slot and written
back with no intervening write to the same list), and they are half the win. The third
writes slot `j` with a write to slot `i` in between, so it needs `i != j` — true, since
`j` runs `i+1..n`, but proving it needs induction-variable range analysis. Build the
conservative version first; it is a much smaller proof obligation for 43% of the run.

**It is a serialisation fix, not a work reduction, and the counters say so.** The partial
version executes *more* instructions than the whole-record one (24.2B against 22.3B) and
takes a third of the cycles (4.8B against 15.2B) — IPC 1.46 -> 5.04. Anything reasoning
about this in terms of "56 bytes versus 24 bytes of traffic" is measuring the wrong thing;
the cost is the reload of a just-stored record failing to forward.

Four plausible fixes were tested first and are all dead ends, recorded so the next person
does not spend the afternoon:

- **The whole-record rebuild is not inherently slow.** Rewriting the *C reference* to
  rebuild the whole struct the way an immutable language must costs **1.01x** — gcc's SRA
  sees straight through it. The problem is that gcc cannot see through *our* version, not
  that the shape is expensive.
- **It is not alignment.** `Body` is 56 bytes, so slots alternate 8/16-byte alignment.
  Padding the record to 64 bytes: **-0.2%**.
- **It is not the out-of-line call.** `neon_list_set_scalar_inplace` is an archive symbol
  while `neon_list_at_scalar` is `static inline` in the header, and the asymmetry looks
  suspicious. Making it inline too: **+0.3%**.
- **It is not bounds checks**, despite their being 1.1B extra branches (5.3x C's branch
  count). Removing them from the generated C entirely made it **9.7% slower**.

**Revised by the clang experiment, same day.** Built clang-all-the-way-down
(`CC=clang`, runtime archive included — verify with `strings` on the archive, not the
binary's `.comment`, which always shows GCC from glibc's crt objects), this benchmark
runs 0.73s against gcc's 2.83s: LLVM's GVN forwards the stored record fields to the
immediate reloads and keeps the body in registers — it performs the partial-record
elimination gcc declines. So the item above is not "the missing 4×"; it is "stop
depending on which C compiler is in a forwarding mood" — explicit field stores would
give both toolchains the fast shape. The toolchain split measured across all four
benches: gcc wins brainfuck by 23% and binary-trees by 10%; clang wins word-frequency
by 5% and n-body by 3.9×. Neither dominates; benchmark tables should say which `cc`
built the Neon row.

---

### 13c. CBMC cannot reach map resize, clone or drop

The heap is modelled as untyped bytes, so a witness release read out of a heap map is a
symbolic function pointer, and CBMC resolves it across every address-taken
`void(*)(void*)` — including the map's own drop — recursing to the unwind bound. One resize
did not finish in 400s; the same harness against a static map finished in 0.25s.

**Unverified as a result:** "resize preserves live entries and drops tombstones", and
copy-on-write at `rc > 1`. Needs `goto-instrument --restrict-function-pointer` in the model
pipeline, or types that distinguish a drop from a witness release.

---

## Later — not now

### 17. `@cfg`'s keys are the HOST, not the target

`Config::for_host` seeds `@cfg` from the machine the compiler is running on, and `--cfg KEY`
adds to that. Neither describes a machine to build *for*. Harmless while nothing
cross-compiles, and not harmless the moment something does: a program built on Linux for
Windows would take every `@cfg(linux)` branch.

The sharp edge is that `--cfg` ADDS. With `--cfg windows` on Linux both keys are live, so
`@cfg(windows)`/`@cfg(not(windows))` pairs select correctly while a
`@cfg(linux)`/`@cfg(windows)` pair keeps both.

Whoever adds `--target` owns this line, and should make it the only way to set the keys —
subsuming `--cfg`, `--cc` and `--runtime`, which together are a target spelled out by hand.

**The cross-build path itself is done** (2026-08-08). `--cfg`, `--cc` and `--runtime` on
`compile`/`run` produce a Windows PE from Linux, and `verify/windows.sh` builds the runtime
with mingw-w64, runs the 90 tinyunit tests under Wine, and builds a Neon program for Windows
and diffs its output against the Linux golden. That is what made `std::fs`'s Win32 half
writable against something that executes rather than only type-checks.

### 19. Derives should eventually live in the stdlib

Owner's direction, recorded so the intent behind `derive.rs`'s shape is not lost. Today a
derive is a Rust unit struct in `DERIVABLE`; the destination is a derive written in Neon, in
the stdlib, next to the protocol it is for — `Display`'s generator belongs beside `Display`.

The registry added 2026-08-03 is the seam for that rather than the answer to it. `Derivable`
is already a narrow interface — a name, and record shape in, declarations out — so a stdlib
mechanism arrives as one more entry in `DERIVABLE` that interprets a generator written in
Neon, not as a rewrite of how `@derive` is wired.

What is actually missing is a language, and it is not small:

- **Compile-time reflection over a record.** A generator needs the field names and types as
  values it can walk. Nothing in Neon can see a type's structure today.
- **A way to produce declarations.** Quoting, or a builder API over `ast`. This is the half
  that turns annotations into a macro system, which `docs/design/annotations.md` and
  `expand.rs`'s `Context` deliberately prevent — a processor may keep or omit its node and
  nothing more. That decision is the one being revisited here, and it should be revisited
  openly rather than eroded: `@derive` is already the exception, split out into a pass for
  exactly this reason.
- **Hygiene.** `to_json_impl` writes `std::json::Json` and `std::json::object` as absolute
  paths precisely so a generated body does not depend on the author's imports. A generator
  written in the stdlib needs that property from the language rather than from the good
  manners of whoever wrote the generator.

Far off, and nothing here blocks anything. The near-term consequence is only that
`Derivable` should stay a narrow interface: a derive that reached for compiler internals
beyond "record shape in, decls out" would be the thing that makes this harder later.

### 18. Model-check the compiler with Kani

The runtime has CBMC models (`runtime/models/`, rules in its README). The compiler is Rust
and gets the same treatment through Kani, which is CBMC underneath.

The shape of what is worth proving is already known from today: the classes that produced
bugs are exactly the ones a model checker is good at. Injectivity of the keys in item 12 is
a proof obligation, not a test — `repr_key(a) == repr_key(b) implies a == b` over
bounded reprs. Same for the block-parameter relation in item 11 once someone defines it,
and for `substitute`'s termination on recursive types.

Owner's call on timing; recorded so it is not lost.

---

## Unproven leads

Marked as such because nobody built a repro. Worth a pass, not worth asserting.

**Four have been investigated and three were real** — L1 (a user `marker Ord` inherited the
built-in rule), L4 (qualified-path impls never matching), L5 (duplicate `TyId`s reaching the
backend), all confirmed and fixed; L2 did not reproduce. That is worth knowing before
treating the rest as noise. Each fix is pinned by a corpus file.

- **L2.** `ordered.rs:90/165` match bare `"List"`/`"Map"`. **No repro, 2026-08-04**: a user
  `record Map { a: i64 }` in its own module is correctly treated as an ordinary ordered
  record. One shape only, so "not reproduced" rather than disproved — the bare-name match is
  still there to be read.
- **L3.** `repr.rs::variant_rank` collapses five variants into one sort rank used as a
  canonical layout ordering. Never probed.
- **L6.** `repr_components` checks `boxed` only on single-atom DNF paths; a multi-atom path
  falls to `record_intersection`, which lays each atom out inline — a second non-termination
  if such a type is constructible. The lead doubts its own constructibility; nobody has
  tried to build one.
- **L7.** `normalize_union([Nullable(Str), Null])` disagrees with `repr_of(str|null|null)`.
  Blocked in the front end today, so it needs a repr-level test rather than a program.

## Environment hazards

Not bugs in the compiler, but they have cost real time and have invalidated evidence.

- **`cargo test | grep FAILED` finding nothing does not mean the tests passed.** A build
  failure prints no `test result` lines at all, so a grep for failure is empty and reads as
  green — which is how a `BuildConfig` initializer in `#[cfg(test)]` code, missing two new
  fields, passed a local check and broke CI. `cargo build` does not compile test targets;
  `cargo test --no-run` does. Count the `^test result: ok` lines (there are 15) rather than
  grepping for the absence of failure.

- **A green local build is not evidence about Windows, or about the linker.** Two CI breaks
  in three days came from taking it as one: `-lm` sat before the archive on the link line
  and glibc 2.34+ resolved `sqrt` anyway, and `cap` was unused inside a `#ifdef _WIN32`
  while the local build carried no `-Werror`. Both were one `cc` invocation from being
  caught. `verify/windows.sh` is that invocation: mingw-w64 builds the archives with
  `-Werror` and the tinyunit suite, and Wine runs it — 90 tests, including the re-exec path
  tinyunit uses for per-test isolation on Windows. It does NOT cover MSVC.

- **The git stat cache is unreliable here.** `git diff` reports a file clean while it holds
  edits, and `git checkout` can be a silent no-op. `git update-index --refresh` fixes it.
  Do not use `git stash` to snapshot; copy files.
- **Filesystem clock skew.** cargo and make report "Finished" without rebuilding. Verify a
  runtime change landed by checking symbols in the archive, not by trusting build output.
- **`/tmp/neon-sysroot/stdlib` is a symlink into the repo.** Doctoring a sysroot writes
  through to the real stdlib. Copy with `cp -rL`.
- **Parallel agents sharing one `target/`** produce unstable results, and a git worktree did
  not provide the isolation it appeared to. Anything proving runtime behaviour needs its own
  `CARGO_TARGET_DIR`.
