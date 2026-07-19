# Open work

Everything known-broken or undecided as of 2026-07-19, distilled from six middle-end
audits, a compiler-wide collapsing-key sweep, three CBMC models and a fuzzing run.

Each item has a repro or a file:line. Nothing here is speculative — the unproven leads are
in their own section at the bottom and marked as such.

---

## P0 — soundness. These miscompile or accept wrong programs.

### 1. Nominal identity is a bare name, so `opaque` is decoration

Two modules declaring the same record name declare the **same type**. No cast, no `any`,
no module-path forgery.

```neon
internal mod vault { opaque record Secret { code: i64 }
                     fn reveal(s: Secret) -> i64 { s.code } }
internal mod forge { opaque record Secret { code: i64 }
                     fn fake(n: i64) -> Secret { Secret { code: n } } }

vault::reveal(forge::fake(99))   // prints 99
```

`typecheck/env.rs::record_body` interns `t.name(&r.name)` — the bare identifier. Every
opacity guarantee rests on this, including `std::fs`'s cleanup guard.

**Not local.** The name is read back by `dispatch::nominal_head` and matched against
`ast_head`'s `path.last()`; `ordered.rs` matches it against literal `"List"`/`"Map"`.
Qualifying the declaration without resolving every written path breaks stdlib dispatch.
`ImplDef.target_head` is already qualified (`env.rs:1263`) while `nominal_head` is bare —
see lead L4, which may mean qualified-path impls never match at all.

Recorded as `tests/lang/types/a_nominal_name_is_not_a_module_identity.neon`, deliberately
unlisted: unlisted+failing is how this ratchet records an open bug.

### 2. Interpolating a dispatched call miscompiles

```neon
"#{area(q)}"        // area(q) resolves through a protocol
```

`check.rs:619` writes the interpolation's `to_string` resolution to the **hole
expression's own `ExprId`**, overwriting that expression's call resolution. Lowering's
`suppress_dispatch` counter then over-suppresses the subtree and the call falls through to
`<todo: path-as-value>` → `call.closure` on a string constant.

Two things keyed on one id. Not fixable in `lower.rs` — the resolution is destroyed before
lowering runs. Needs `set_call` keyed on something other than the hole's id.

### 3. A bare type name in a match arm is silently an irrefutable binder

```neon
match x { A => ..., B => ... }    // on x: A | B
```

Lowers to `block0: jump block2` — arm 1 unconditionally, arm 2 dead, no diagnostic. `A` is
parsed as an identifier pattern shadowing the type name. `is A =>` is correct. The binder
semantics may be intended; the second arm silently becoming unreachable is not.

### 4. Erasing a narrowed union member tags the box with the union

```neon
fn e(x: A | Node) -> any { match x { is A => x as A, is Node => x as Node } }
```

The match join block is typed at the union, so the implicit erasure at `ret` boxes with
`type_tag(union)`. Both `p is A` and `q is Node` come back **false** on values that are
exactly those types. Refcounting on that path is correct; only the tag is wrong.

### 5. Unsolved generics reach codegen as an ICE

```neon
fn make[T]() -> List[T] { list::new() }
let a = make();      // internal error: type variable 'T reached codegen
```

`solve_generics` is first-wins and silently returns what it managed; `direct_call`
substitutes anyway without checking coverage. Fix is mechanical — after `solve_generics`,
substitute the unsolved names with poison and compare against the real substitution; if
`sig.ret` changes it mentioned an unpinned variable — but it needs a new
`TypeErrorKind::CannotInferTypeParam`, since every existing variant misdescribes it.

### 6. Default protocol method bodies are never type-checked

`check.rs:217` calls `fn_body(module, m, &[])`, so the protocol's subject is unbound and
any `T` in the body is `unknown type T`. Also leaks `#error` into a follow-on message.

### 7. `Resolution::Bound` on a union receiver prints a compiler marker as program output

```neon
fn show[T](v: T) -> str { "#{v}" }    // at T = A | B
```

Compiles clean, exits 0, prints `<todo: bound: abstract receiver>`. `repr_head` returns
`None` for a union. Needs the variant-switch machinery — a feature, not a fix, and the same
gap as `Resolution::Switch`.

### 8. Reading a field off a record whose recursion runs through `List`

Reports ``L has no field `xs` `` for a field that is declared. Construction works; only the
read fails.

---

## P1 — structural. These are why P0 items keep appearing.

### 9. `narrow.rs` has zero callers

The safety module — `Refined` deliberately has no `then_ty` on the impossible case,
`Projected` deliberately has no `never`, ~40 passing unit tests, and a module doc
explaining at length the exact unsoundness that was live today. `match_expr` reimplements
narrowing inline with a raw `intersect`; `if`/`while` don't narrow at all.

A green suite over a disconnected module reads exactly like a green suite over a connected
one. Decide whether `match_expr` calls `narrow::narrow`/`redundant_arms`, and whether
`if`/`while` should narrow. Right now the module encoding the soundness argument and the
code making the decisions are two different programs.

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

Fixed today: `repr_key`, `type_tag_name` (three separate times), `field_name`, closure tags.
Still open: `repr_from_typespec` drops type arguments so `ident[Box[i64]]` and
`ident[Box[str]]` collide (currently caught by gcc — "correct by coincidence");
`impl_head`'s `_ => String::new()` makes two tuple impls collide into one symbol.

The sweep's own verdict: *"I kept finding more, and the rate did not fall."* Each fix pushed
the question one layer up — fix the tag, the repr feeding it is collapsed; fix the repr, the
type it came from is collapsed. It terminates at whatever the compiler treats as a primitive
name, which is item 1.

**Tell, for future readers:** a `match` over a structured type whose arms return string or
integer constants, where the result is used as a name, key or tag. Every such function
should carry an injectivity obligation in its doc — *backed by an assertion, not prose*.

### 13. Stdlib diagnostics render against the user's file at a fabricated location

An error injected into `std/io.neon` printed with the **user's** path, underlining `}` on
line 4. With 40 lines of padding the same error moved to line 17, inside a comment.
`check_all` sorts every module's errors by raw span offset and one `Renderer` holds one
file. `TypeError` needs a file id.

This is why a stdlib mistake produces a baffling diagnostic pointing at a test's closing
brace — it has cost several people time today.

---

## P2 — decisions. These need an owner's call, not an implementation.

### 14. Should `any` hold a container?

`let a: any = [1, 2, 3]` works now. If it should be a compile error, that is a small change
today and a large one later. The answer also decides `List[any]` and `Map[str, any]`.

### 15. Should `as` be checked?

`as` is an unchecked reinterpretation everywhere: `null as str` yields `""`, and
`(x: i64|str) as str` on an i64 reads garbage. The checker is right not to reject these —
`as` exists to assert what the checker cannot prove — but the assertion is never
*discharged*. It is a reinterpret cast wearing a checked cast's name. Making it trap is a
language decision with a cost on every narrowing.

### 16. Should block comments exist?

They nest, deliberately and correctly — commenting out a region containing `*/` must not
end early. But `//` plus an editor covers the use case, and dropping them removes the
tree-sitter external scanner entirely (nesting is why it exists).

### 17. Move `List`/`Map` out of the prelude

`@runtime` makes this possible now. It also removes the prelude-vs-root collision that
forces `opacity_permits` to treat the root as a non-container — see the exception in
`check.rs::opacity_permits`.

---

## P3 — cleanup

- `docs/design/ir.md` refers to `rt.h` in three places; it no longer exists (the umbrella is
  `libneon_rt.h`).
- `docs/design/resources.md` is stale three ways: the throwing-closure prerequisite is met,
  cleanup is `(T) throws E -> null` (`()` is not a type in this language), and `File` is
  implemented.
- `compiler/src/backend/c.rs::throwing_call_results` — dead, referenced nowhere.
- `lexer/error.rs::UnmatchedCloseBrace` — never constructed.
- `parser/mod.rs::fn_like`'s `body_required` parameter is unread; call sites pass it
  meaningfully. Deliberate or oversight, unknown.
- `tests/lang/records/spread_with_override.neon` is `known-bug`: `P { y: 9, ..a }` does not
  parse, because `allow_trailing()` on the field list eats the comma the spread needs.

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

- **L1.** `env.rs::satisfies_marker` matches the bare protocol name `"Ord"`, so a user
  `marker Ord` in any module may inherit the built-in rule.
- **L2.** `ordered.rs:90/165` match bare `"List"`/`"Map"`.
- **L3.** `repr.rs::variant_rank` collapses five variants into one sort rank used as a
  canonical layout ordering.
- **L4.** `ImplDef.target_head` is qualified while `nominal_head` is bare — **qualified-path
  impls may currently never match at all.**
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
