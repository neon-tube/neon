# TODO

Everything known-broken or undecided as of 2026-07-22, distilled from six middle-end
audits, a compiler-wide collapsing-key sweep, the CBMC models and a fuzzing run.
Resolved items are removed, not struck through — their write-ups live in the design docs
they produced (`docs/design/identity.md`, `docs/design/opacity.md`,
`docs/design/checked-casts.md`).

Each item has a repro or a file:line. Nothing here is speculative — the unproven leads are
in their own section at the bottom and marked as such.

---

## P0 — soundness. These miscompile or accept wrong programs.

### 7. `Resolution::Bound` on a union receiver prints a compiler marker as program output

```neon
fn show[T](v: T) -> str { "#{v}" }    // at T = A | B
```

Compiles clean, exits 0, prints `<todo: bound: abstract receiver>`. `repr_head` returns
`None` for a union. Needs the variant-switch machinery — a feature, not a fix, and the same
gap as `Resolution::Switch`.

### 7b. Generic impls never apply

```neon
impl[T] Tag for Pair[T] { ... }
Tag::tag(p)                      // no impl of `Tag` for `Pair[i64]`
```

The target's `T` is rigid, so the emptiness query that decides applicability is always
empty. `ImplDef.generics` is stored and **consumed by nothing**. A whole feature that
parses, type-checks its own body, and never matches.

Bounded impls do not parse at all: `ast::ImplDecl` has no `wheres` and `parser::impl_decl`
has no `where`. `dispatch.md` describes both as design rather than as built.

### 7c. `Resolution::Switch` prints a compiler marker as program output

A two-impl union receiver compiles, exits 0, and prints `<todo: dispatch switch>`. Same
family as item 7 and wants the same variant-switch machinery.

### 8b. `test` blocks are silently inert

```neon
test "arithmetic" { assert(1 + 1 == 3); }   // compiles; the program prints "main ran"
```

The block parses and type-checks. The failing assert does nothing, and there is no
`neon test` verb in `cli/src/cmd/`. The language has testing syntax that runs nothing —
`decisions.md` chose assert intrinsics over a library specifically for the reporting they
would give, and that reporting does not exist.

### 8d. An `if`/`match` join into `(union, scalar)` mislays the scalar

```neon
type J = null | bool | i64 | f64 | str
fn f(cond: bool) -> (J, i64) { if cond { (true, 4) } else { (null, 5) } }
let (v, k) = f(true);      // k is 0, not 4 — compiles, exits 0, wrong data
```

The two arms build tuples whose first elements are *different* members of the union (`bool`
vs `null`), so each arm boxes that element at its own repr; the join adopts one arm's tuple
layout and the `i64` tail is then read at the wrong offset for the other arm's value. The
same disease canonical tags fixed at the `any` boundary (docs/design/checked-casts.md,
decision 5) — a union laid out by one arm and read by another — still live at tuple joins.

Not triggered when the arms agree: `(true, 4)`/`(false, 5)` is correct, and a single-arm
`(true, 4)` at `(J, i64)` is correct. Recursion is not required — the union above is a plain
`type`. Widening each arm's element to the union *before* the tuple (one `J`-typed binding,
one tuple built after the join) is correct, as is returning a record instead of a tuple.
Found writing a DOM JSON parser, where every `parse_value` arm returns `(Json, next)`.

---

## P1 — structural. These are why P0 items keep appearing.

### 9. `match_expr` still narrows inline; `redundant_arms`/`residual` still unused

Half-resolved 2026-07-22: `if`/`while` now narrow through `narrow::narrow_is` /
`narrow_null` / `Refined` (`check.rs::cond_refinements`), assignment dissolves
refinements against the declared type, and the value-side projection lives at
`lower.rs::lower_path` — pinned by `control/narrowing_in_if_and_while.neon` and
`match/narrowed_scrutinee_flows_bare.neon`. What remains of the original item:
`match_expr` still reimplements its narrowing with raw `intersect`/`diff` rather than
calling `narrow::narrow`, and `residual`/`is_exhaustive`/`redundant_arms` still have no
callers — the exhaustiveness logic exists twice. A refactor, not a bug: the inline copy
is currently correct.

Known limit of the wired half, recorded in `docs/design/checked-casts.md`: the
else-branch of an `any` subject deliberately does not refine (`any \ T` is a complement
with no repr of its own), and only bare locals narrow — a field or call result has no
binding to shadow.

### 10. The `ir_lower.rs` guards are aimed at a program the compiler never builds

`no_type_variable_survives_lowering` and `any_never_appears_unless_the_source_type_is_any`
are non-vacuous but:

- they lower with `libs = &[]` — the real pipeline adds **13,522 functions per corpus file**
  they have never looked at;
- they use `stdlib::parse` + `check_module` where the pipeline uses `parse_from(.., 0)` +
  `number_exprs_from` + `check_all`, so `ExprId`s collide and stdlib bodies go unchecked;
- they scan `f.values()` only — never `f.ret`, `f.throws`, `f.env`, `Op::IsVariant::tested`,
  `program.recursive` or `program.boxed`;
- `any_never_appears` tests the top level only while its name claims the nested property.

Rebuilt correctly the answer is still 0, so this is latent, not live. Align the harness
with `cli/src/frontend.rs`.

### 11. The block-parameter repr invariant is *undefined*, not merely unchecked

`ssa.rs` says predecessors pass arguments in parameter order. It does not say what relation
the **reprs** must satisfy. It is not equality — a verifier asserting equality flags
**9,226 sites** across the corpus (`str` and `Null` into a `str?` join, bare `i64` into an
`i64 | null` parameter), and every one of those programs runs correctly, so the emitter is
widening. The real invariant is "assignable", and that relation exists nowhere — not as a
function, not as a doc. No verifier can be written until someone defines it.

This is the shape that *precedes* a Class B bug rather than an instance of one.

### 12. Collapsing keys — the class has no bottom yet

A lossy projection used as an identity. Not a fallback: these functions are total and every
arm is correct *as a description*; the codomain is just smaller than the domain.

Fixed in the 2026-07-20 sweep: `repr_key`, `type_tag_name` (three separate times), `field_name`, closure tags.
Still open: `repr_from_typespec` drops type arguments so `ident[Box[i64]]` and
`ident[Box[str]]` collide (currently caught by gcc — "correct by coincidence");
`impl_head`'s `_ => String::new()` makes two tuple impls collide into one symbol.

The sweep's own verdict: *"I kept finding more, and the rate did not fall."* Each fix pushed
the question one layer up — fix the tag, the repr feeding it is collapsed; fix the repr, the
type it came from is collapsed. It terminates at whatever the compiler treats as a primitive
name — which, since the identity fix (`docs/design/identity.md`), is the qualified
declaration key rather than a bare string, so the class now has its bottom.

**Tell, for future readers:** a `match` over a structured type whose arms return string or
integer constants, where the result is used as a name, key or tag. Every such function
should carry an injectivity obligation in its doc — *backed by an assertion, not prose*.

### 13. Stdlib diagnostics render against the user's file at a fabricated location

An error injected into `std/io.neon` printed with the **user's** path, underlining `}` on
line 4. With 40 lines of padding the same error moved to line 17, inside a comment.
`check_all` sorts every module's errors by raw span offset and one `Renderer` holds one
file. `TypeError` needs a file id.

This is why a stdlib mistake produces a baffling diagnostic pointing at a test's closing
brace — it has cost real time, repeatedly.

### 19. No diagnostics channel survives monomorphization

The checker checks a generic body once, with its parameters rigid; instantiation happens
at lowering, which cannot report. Two known consequences, both from the checked-casts
work (`docs/design/checked-casts.md`):

- the `sealed` assertion ban does not fire through a generic launderer
  (`fn f[T](a: any) -> T { a as! T }` at `T = Secret`) — off-ratchet pin
  `records/sealed_no_generic_assert_laundering.neon`; soundness-guarded meanwhile by the
  canonical tag check;
- a type parameter mentioned only in `throws` still reaches codegen as the "type
  variable 'T" ICE — the former §5 fix promotes candidates against the final *recorded*
  type, and throws types are not recorded per expression.

Either per-instance re-checking or a lowering-side error path closes both.

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

## P3 — cleanup

- `docs/design/ir.md` refers to `rt.h` in one remaining place; it no longer exists (the
  umbrella is `libneon_rt.h`).
- `docs/design/resources.md` is stale three ways: the throwing-closure prerequisite is met,
  cleanup is `(T) throws E -> null` (`()` is not a type in this language), and `File` is
  implemented.
- `lexer/error.rs::UnmatchedCloseBrace` — never constructed.

---

### 13a. Code comments contradicted by the code they document

Found while rewriting the design docs; all flagged rather than fixed.

- `ir/repr.rs` — `Repr::Map`'s doc says "an immutable HAMT". `runtime/src/map.c` is an
  open-addressed table with control bytes, copy-on-write above `rc > 1`.
- `NEON_IMMORTAL` is read by `neon_retain`/`neon_release` and **set by nothing**. Either
  string literals are not actually immortal, or the flag is dead.
- `backend/c.rs::emit_term` — `Term::Throw` in a non-throwing function emits
  `neon_panic(var(v))` with the value raw, while `neon_panic` takes a `neon_str`. Whether
  lowering can produce that shape is unconfirmed.

### 13b. The stdlib never goes through `expand`

So a stdlib `@cfg` is silently ignored, and a typo'd stdlib annotation is undiagnosed.
`@runtime` and `@pure` still work because their readers go to the AST rather than to
`Meta`. No reasoning for this exists anywhere in the code.

### 13c. CBMC cannot reach map resize, clone or drop

The heap is modelled as untyped bytes, so a witness release read out of a heap map is a
symbolic function pointer, and CBMC resolves it across every address-taken
`void(*)(void*)` — including the map's own drop — recursing to the unwind bound. One resize
did not finish in 400s; the same harness against a static map finished in 0.25s.

**Unverified as a result:** "resize preserves live entries and drops tombstones", and
copy-on-write at `rc > 1`. Needs `goto-instrument --restrict-function-pointer` in the model
pipeline, or types that distinguish a drop from a witness release.

---

## Serialization — completing protocol dispatch

A stdlib JSON module wants `protocol Serialize`/`Deserialize` with library impls for the
recursive cases and a derive for records — the serde shape, but resolved statically at
monomorphisation so there are no dictionaries. None of it is expressible until dispatch is
finished. The concrete pieces, in order, each closing an item already listed above:

1. **Lower `Resolution::Switch`** (item 7c) and **`Resolution::Bound` on a union receiver**
   (item 7). The checker already computes the arms — `dispatch.rs:243`, coverage-checked and
   most-specific-filtered — and lowering throws them away at `lower.rs:1975`. Emit a
   `Term::Switch` on the receiver's discriminant, one arm per `(TyId, ImplId)`, reusing the
   runtime tag test that `match`/`is` narrowing already emits. This is the single primitive
   under both union *encode* and union *decode*, so it earns its keep independent of JSON.

2. **Parse `where` on impls.** `ast::ImplDecl` has no `wheres` and `parser::impl_decl` has no
   `where` clause. Cheap; unblocks bounded impls, which do not parse today.

3. **Impl-head unification, not intersection** (item 7b). Applicability at `dispatch.rs:220`
   does `intersect(receiver, target)`, so `impl[T] Serialize for List[T]` never matches — its
   `T` is rigid, the meet is empty. Treat the impl's own generics as flexible holes and
   *match* the receiver against the head, yielding a substitution (`T ↦ i64`).
   `ImplDef.generics` is stored and consumed by nothing today; this is what consumes it.

4. **Discharge the context under the subst.** With `T ↦ i64`, `where T: Serialize` becomes a
   subgoal resolved by another applicability query → `Direct(impl Serialize for i64)`. It
   terminates because the type shrinks structurally. This is the existing `Bound` path, fired
   at instance-lowering time when the impl itself is generic.

5. **Records need a derive.** Bounded impls handle `List`/`Map`/tuples/unions; they cannot
   iterate arbitrary named record fields, and there are no macros. `@derive(Serialize)`
   generates an ordinary `impl` per record via the same structural walk the compiler already
   does for `to_string`/`==`. The walk is shared by every derivable protocol, not
   JSON-specific — the one irreducible bit of compiler magic, and it produces a normal,
   overridable impl rather than a baked-in special case.

**Litmus test for "done":** delete the baked-in structural `to_string`/`==`/`cmp` walks for
containers and replace them with `impl[T] Display for List[T] where T: Display` (and the
Map/tuple analogues). If library impls can express what the compiler currently hardcodes,
the machinery is complete and JSON falls out as one more protocol with zero JSON-specific
compiler code. If they can't, a piece above is still missing.

Union *decode* has one extra obligation the encode side doesn't: choosing an arm. Use the
emptiness checker (`solver.is_empty`, already the basis of `dispatch.rs:220`) to require the
arms' JSON projections be pairwise disjoint, and reject as a compile error when they overlap
("union not unambiguously decodable, add a tag") rather than silently picking arm order.

---

## Later — not now

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
(L4 — qualified-path impls never matching — graduated: confirmed real and fixed with the
identity change.)

- **L1.** `env.rs::satisfies_marker` matches the bare protocol name `"Ord"`, so a user
  `marker Ord` in any module may inherit the built-in rule.
- **L2.** `ordered.rs:90/165` match bare `"List"`/`"Map"`.
- **L3.** `repr.rs::variant_rank` collapses five variants into one sort rank used as a
  canonical layout ordering.
- **L5.** Deferred-op duplicate `TyId`s reaching the backend, where `repr.rs`/`ctype.rs` key
  on `HashMap<TyId, _>`.
- **L6.** `repr_components` checks `boxed` only on single-atom DNF paths; a multi-atom path
  falls to `record_intersection`, which lays each atom out inline — a second
  non-termination if such a type is constructible.
- **L7.** `normalize_union([Nullable(Str), Null])` disagrees with `repr_of(str|null|null)`.
  Blocked in the front end today; the repr-level defect is real.
- **L8.** `is_equatable` rejects a union of two records. The obvious relaxation is *not*
  sufficient — the second BDD path carries a negative — and whether the backend's tag-routed
  comparison would be correct is unverified.

---

## Environment hazards

Not bugs in the compiler, but they have cost real time and have invalidated evidence.

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
