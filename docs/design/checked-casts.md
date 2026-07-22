# Checked casts and `sealed`

**Status: accepted and IMPLEMENTED 2026-07-22** — all ten decisions made and built the
same day, in dependency order: canonical tags (former TODO §4), narrowing (former TODO
§9's if/while half), the triad, the `sealed` split. Grew out of closing TODO §1 (`as`
out of `any` never checks the tag). Resolves TODO §§14 and 15 and closes opacity.md
residues 1 and 2.

The generic-instantiation residue closed 2026-07-22: a signature records which of its
own type parameters the body asserts with `as!` (`FnSig::asserts`, collected
syntactically at declaration), and every call site discharges the obligation against
the solved substitution — binding an asserted parameter to a type with a sealed leaf
foreign to the callee's module is rejected at the call, which is where the
monomorphised assertion comes into existence. Direct spelling only: a wrapper that
forwards to another generic's `as!` is not traced, and that depth stays guarded by the
canonical tag check at run time. Pinned on-ratchet as
`records/sealed_no_generic_assert_laundering.neon`.

Implementation landmarks, for the next reader: canonical erasure/recovery in
`backend/c.rs::coerce_expr`/`unbox_expr`; narrowing in `check.rs::cond_refinements` /
`refinement_pair` / `DefKind::Refinement`, with the value-side projection at
`lower.rs::lower_path`; the trichotomy in `check.rs`'s `ExprKind::As` arm; `as?`/`as!`
lowering in `lower.rs::lower_cast_soften`/`lower_cast_assert`; the sealed sets in
`env.rs::foreign_sealed_tags` and the ban/Ord bar via `check.rs::sealed_leaf`.

## The problem, in three facts

1. **`as` is unchecked everywhere** (TODO §15): `null as str` yields `""`, a union
   narrowed to the wrong arm reads garbage, and until this week `as`-from-`any`
   reinterpreted the box payload at whatever type the source claimed. An assertion
   that is never discharged — a reinterpret cast wearing a checked cast's name.
2. **The `any` forge cannot be closed by rejecting the flow.** `any` legitimately holds
   opaques (widening in is legal), so a cast out of `any` is statically
   indistinguishable from erased recovery. Established empirically 2026-07-20: the
   strict rule broke five corpus files (opacity.md, residue 1). What that experiment
   actually proved: rejection *without an alternative spelling* is impossible — not
   that static rejection is impossible.
3. **A bare cast that traps is invisible fallibility.** It reads like a coercion the
   checker vouched for and dies like an assertion. Unlike `xs[i]`, there is no
   cultural prior that `as` can miss.

## Already landed (the substrate — kept under every option)

`as`-from-`any` now compares the box tag against the target's tag and traps on
mismatch: `neon_box_expect` in `runtime/src/any.c`, emitted by `cast_expr` in
`backend/c.rs`. Honest recovery passes (tags match by construction); forgery and
wrong-type recovery trap. Pinned by `records/opaque_no_any_laundering.neon` (forge
traps, read stays dead) and `types/any_cast_checks_the_tag.neon` (general, non-opaque
case); model `box-expect-traps-on-a-mismatched-tag` proves no mismatched cast returns.

This is sound but keeps the bare spelling — fact 3 stands. The two layers below are
about the spelling, and both compile down to this check.

## Layer 1: the cast triad

The language already solved "fallible operation, invisible at the call site" once, for
throwing calls (`docs/design/errors.md`): a bare call is a compile error; you write
`try` (propagate), `try?` (soften to `T | null`), or `try!` (assert, trap). Apply the
same discipline to casts:

| form | meaning | on mismatch |
|---|---|---|
| `as` | infallible coercions only: widening into a union or `any`, newtype wrap/unwrap, narrows the checker can prove | cannot mismatch; fallible use is a **compile error** |
| `as? T` | test; yields `T \| null`, composes with `orelse` | `null` |
| `as! T` | assertion | trap (the landed tag check) |

One rule replaces "unchecked, except…": **`as` never lies; `?` and `!` mark the risk.**
This covers unions and nullables too, not just `any` — the union's discriminant is
already in the value, so `(x: i64 | str) as! str` is one tag compare, and item 15's
`null as str -> ""` becomes: bare `as` illegal (not infallible), `as!` traps, `as?` is
honestly always-null.

## Layer 2: split `opaque` into `opaque` and `sealed`

Today one keyword bundles two properties. Four quadrants exist and only three are
expressible:

- **`opaque` — the representation is not part of the interface.** All of today's
  structural gates: no structural views, construction, probes, or field reads.
  Assertion is *allowed*: `a as! List[i64]` is legal anywhere and tag-checked.
  Rationale: possession of a List carries no privilege — anyone can build `[1,2,3]`
  legally — so asserting recovery claims nothing sensitive. This is `List` and `Map`.
- **`sealed` — opaque, plus a trust boundary.** Everything above, and additionally:
  `as!`/bare `as` naming the type outside the owner's subtree is a **compile error**,
  regardless of source. Recovery outside the owner is only `as?`/`is`. The value's
  existence certifies the owner's invariants (a `File` is a cleanup guard in a field),
  so an outsider asserting one is at best redundant — they could test — and at worst
  the bug. This is `Secret`-style types, `Resource`, `File`.
- Plain records: neither. Native + trust: `sealed` on a runtime-backed type.

**Safety constraint, non-negotiable:** the structural-cast door stays shut on *both*
knobs. A concrete (non-`any`) source has no box and no tag — `({}) as! List[i64]` from
a structural value would be an unguarded reinterpret into native list memory. The
static gates guard the concrete path; the tag check guards the erased path; the split
never trades one for the other.

**`sealed` and native-ness are orthogonal.** Do not define `opaque` as "runtime
provided" — that is what `@runtime` and an empty body already say, and binding them
would leave user types wanting representation-privacy-without-trust (a `Config` hiding
fields for refactoring freedom) with no spelling.

**What `sealed` does not add: construction protection. It is identical under both
knobs.** Every forge route on an opaque `Config` is already dead:

| route | killed by |
|---|---|
| `Config { retries: 3 }` outside the owner | the literal door (`opaque`'s structural gates) |
| `({retries: 3}) as Config`, concrete source | the cast gates (`opaque`'s structural gates) |
| structural value boxed into `any`, then `as! Config` | the tag check — tags are stamped only at genuine construction, so the box never carries `Config`'s tag; the cast traps |

The *only* thing `sealed` removes is an outsider's assertive **recovery** of a genuine
value — one the owner really built, held legally, re-typed out of `any` with `as!`
instead of `as?`. That is not an integrity property; it is a policy about who may
claim to know what an erased value is, and whether a wrong claim is a compile error or
a disclosed trap.

The concrete case for opaque-not-sealed — the service registry, heterogeneous
storage's daily driver:

    let services: Map[str, any] = map::set(services, "config", config::load("app.toml"));
    // far away, in a module the config author has never seen:
    let cfg = services["config"] as! config::Config;

That `as!` discharges knowledge about the *map slot* ("I know what boot stored under
this key" — a contract between the storing and reading modules), not about Config's
internals. It is safe: a wrong slot traps on that line like `xs[i]` out of bounds; only
a genuine Config can succeed, since nothing else carries its tag. Sealed forbids the
spelling, and the forced `as? … orelse` has no honest filler — a mistyped slot is a
wiring bug, so the choices are a throwing wrapper (ceremony up the whole stack for an
unrecoverable condition), a default that masks the bug, or a hand-rolled worse `as!`.
Sealing a data type changes no attacker's options, only honest users' spelling. The
same registry pattern on a `File` is exactly what `sealed` deliberately spends — for a
capability, misidentifying one should be inexpressible outside the owner (and that
trade is open question 3).

So the knob choice is: *is outsider assertion legitimate for this type?* Rule of
thumb: **seal capabilities, not data.** If someone's `as!` on the type trapping in
production would be a security incident (`File`, `Resource`, a token — the value's
existence is a privilege), seal it. If it would be an ordinary bug (`Config`, a data
record with hidden layout — recovery from heterogeneous storage is a legitimate,
common operation), `opaque` already provides everything; sealing it is not unsound,
just a ceremony tax on users with no integrity return.

What the checker knows statically, and what it cannot: it recognizes the *shape*
(source `any`, target foreign-sealed) — that predicate drives the `as!` ban. It cannot
distinguish honest recovery from fishing; that difference is the dynamic value, which
`any` erases. It does not need to: **tests cannot forge.** Tags are stamped only at
genuine construction/erasure, so a structural `{code: 99}` boxed into `any` never
carries `Secret`'s tag — `is` answers false, `as?` answers null. The residual channel
is one bit per test, the already-accepted residue.

## Resulting semantics (target × form × location)

| | inside owner subtree | outside |
|---|---|---|
| `as` (infallible) | ✓ | ✓ |
| `as!` / bare fallible `as`, target foreign **sealed** | ✓ (tag-checked) | ✗ compile error |
| `as!`, target **opaque** or plain | ✓ (tag-checked, traps) | same |
| `as?`, any target | ✓ | ✓ (forge → null) |
| `is`, naming a type | ✓ | ✓ (forge → false; already pinned legal) |

Division of labor: **opacity/trust enforcement is fully compile-time** (the `any` route
was the lone runtime-enforced route in opacity.md's table; the `sealed` ban closes that
asymmetry). **Cast soundness is runtime** (the tag check), because which casts succeed
is dynamic by construction.

## Migration

- Corpus: the erased-recovery files respell `as` → `as!` (or `as?` + `orelse`);
  capability-flavored `opaque_*` tests respell `opaque` → `sealed`; `List`/`Map` stay
  `opaque`. Deliberate spec change — the corpus is the spec; the ratchet keeps it honest.
- Stdlib: `File`/`Resource` → `sealed`; collections unchanged.
- Enforcement site: `check.rs::opaque_view`'s source-must-vouch rule — the branch that
  exempts `any` as "broad enough to have held one" is where the `sealed` ban lands;
  `foreign_opaque_tags` splits into two per-module sets (sealed ⊆ opaque).

## Decisions (2026-07-22)

1. **`sealed` implies `opaque`.** The explicit pair `sealed opaque` is permitted —
   redundancy that documents.
2. **`Ord` is barred for foreign `sealed` types; `Eq` stays; `opaque` is unaffected.**
   Closes opacity.md residue 2 (ordering as a contents oracle) at its natural home —
   barring `Ord` on List was always absurd; on `sealed` types it is coherent.
3. **`sealed` does not bar `is`/`as?`** — tests stay legal so long as they cannot
   forge, and they cannot: a tag is stamped exclusively at genuine
   construction/erasure, so a test only ever *recognizes* a value the owner really
   built. The one-bit probe per test is the accepted residue.
4. **`narrow.rs` gets wired (TODO §9) as part of this work.** `if a is T { … }`
   refines the subject, so guarded uses need no cast at all — bare `as` inside a guard
   is plain subsumption — and the triad never forces a ceremonial `!` on a proven
   path. This was the triad's largest ergonomic cost; the decision removes it by
   landing the two together.
5. **Canonical tags (TODO §4) land with the triad as one semantic unit.** Erasing through a union join
   stamps the box with `type_tag(union)`, so the same logical value carries a
   different tag depending on which erasure site boxed it. Today that is an
   incoherence (`e(a) is A` false on a genuine `A`); under the triad it breaks the
   advertised contract — `e(a) as! A` *falsely traps* on a genuine `A`, `as?` gives a
   false null — and undermines the forgery argument itself, which rests on "a tag is a
   function of the value's concrete type." Proposed fix: erasing a union-typed value
   switches on the discriminant and boxes the projected member with the *member's*
   tag (a null member boxes as null), making every box concretely tagged.
   Consequence to build alongside: casts from `any` to a *union target* become a tag
   membership check plus injection into the union repr (`a as! (A | B)`: tag ∈
   {tag(A), tag(B)}, then inject; `is (A | B)` is the same disjunction).
6. **The `sealed` assertion ban fires post-monomorphization.**
   `fn f[T](a: any) -> T { a as! T }` at `T = Secret` outside the owner is a compile
   error at the instantiation. Pin alongside `opaque_no_generic_laundering.neon`.
7. **`as?` is rejected where the target overlaps `null`.** Diagnostic: *"can't infer
   null-as-value vs null-as-failure"*.
8. **The trichotomy is the infallibility rule for bare `as`.** Every cast classifies
   into exactly three classes, each with one legal spelling:

   | class | meaning | spelling |
   |---|---|---|
   | always succeeds | subsumption (`actual <: target`, post-sealing-gates); widening into a union or `any`; newtype wrap/unwrap | bare `as` |
   | might succeed | source and target overlap, neither contains the other: `any` → concrete, union → member, `str?` → `str` | `as?` / `as!` |
   | never succeeds | no overlap: `(x: i64) as str`, `null as str` | compile error in every spelling — `as!` would be a provably-always-trap |

   "Infallible" is not a bespoke list: it is the *always* class, computed from the
   subtype relation the checker already has, plus the two non-subtyping bijections
   (boxing, newtype `#inner`). Infallibility is necessary, not sufficient — the
   opacity/sealing gates still apply on top. With narrowing wired (decision 4),
   guard-refined uses land in the *always* class via subsumption. TODO §15's zombie
   cases resolve: `null as str` was *never*-class garbage that happened to yield `""`;
   it stops compiling. During implementation the *always* class gets written down
   precisely against `opaque_view`'s rules; the trichotomy is the acceptance boundary.
9. **Keywords: `opaque` / `sealed`.** `opaque` is the term of art for exactly the weak
   knob's semantics — C's opaque pointers/handles, TypeScript/Flow's opaque types,
   ML's abstract types (`abstract` itself unusable; OO poisoned it) — and the split
   restores that traditional meaning by moving the trust connotation onto `sealed`.
   `sealed` over `secret` for the strong knob: it names a boundary against outside
   *claims*, not confidentiality (contents-hiding is already `opaque`'s job), reads
   correctly on `File`/`Resource` where `secret` reads wrong (a handle is privileged,
   not confidential), and matches `Types::seal`, the literal enforcement mechanism.
   Wrinkle kept documented: `seal()` runs for all opaques; the keyword marks the
   subset with the assertion ban. Swift's `some P` and C#/Java `sealed` priors are
   different concepts in non-competing positions.
10. **`any` and containers stay legal in both directions (resolves TODO §14: keep).**
    Context that led here: orthogonal to this
    design's shape — the tag check, triad and split are motivated by records and
    scalars regardless; §14 only scales how often the fallible forms are used. But two
    couplings, recorded rather than silently decided: (a) this design *removes §14's
    strongest argument* — before the tag check, container recovery was a memory-safety
    hazard (`a as List[str]` on a `List[i64]` box reinterpreted integers as string
    pointers); now it traps cleanly, and `List[i64]`/`List[str]` carry distinct tags.
    What remained for barring was philosophy, not soundness. (b) The service-registry
    example motivating opaque-not-sealed is a `Map[str, any]`; barring `any` as an
    element type would have killed that pattern and deleted the five recovery corpus
    files rather than respelling them.

## Acceptance

- The triad parses; bare fallible `as` is a diagnostic naming the two repairs; a
  never-class cast is a diagnostic in every spelling; `as?` against a null-overlapping
  target says "can't infer null-as-value vs null-as-failure".
- `records/opaque_no_any_laundering.neon` splits: the forge in `as!` spelling is a
  compile-fail file (the `sealed` ban); the `as?`/`is` spellings get a runtime file
  proving forge-yields-null/false. The generic-instantiation ban (decision 6) gets its
  own compile-fail pin.
- The five recovery files respell and stay green; `types/any_cast_checks_the_tag.neon`
  respells to `as!` and stays the general-soundness pin.
- Narrowing (decision 4): `if a is T { … }` uses the subject at `T` with no cast; a
  corpus file pins it, and the `| null` ergonomic gap noted in TODO §8e resolves.
- Canonical tags (item 5, if accepted): a corpus file pins that a union-erased member
  answers `is`/`as?`/`as!` identically to a directly-erased one.
- `Ord` on a foreign `sealed` type is a compile-fail pin; `Eq` stays legal.
- Corpus green under ASan; the CBMC model unchanged (the substrate did not move).
