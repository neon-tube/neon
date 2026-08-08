# Design: the type checker

**Status:** built. `compiler/src/typecheck/` is the implementation; this file is the
argument for its shape, and the record of where the shape and the code disagree.

Two rules for reading it:

- The **module docs are more current than this file** and are not duplicated here. Where
  a section says "see `types.rs`", read `types.rs` — a second copy is a second thing to
  drift.
- Anything marked **Does not hold** is a property this document once asserted and the
  code does not have. It is left in, marked, because the failure mode this project spent
  a day chasing was exactly the other choice: prose asserting a property that had stopped
  being true, and readers trusting it. Open items point at `TODO.md`, by section name
  rather than by number — numbers were cited here for items that were later removed, which
  left readers chasing sections that no longer existed.

## The bet

Types are **sets of values**. Union, intersection and negation mean what they mean in set
theory, and subtyping is containment:

    s <: t   ⟺   s ∧ ¬t  is empty

So the whole checker rests on one question — **is this type empty?** — and everything else
is bookkeeping. This is the Frisch–Castagna–Benzaken semantic-subtyping approach (CDuce,
and Castagna's later work on union/intersection/negation types).

It is a real bet. It buys `T | null` with no Option, `{name: str}` structural parameters
that nominal records satisfy, exhaustiveness that falls out of `s ∧ ¬covered = ∅`, and
`:ok | :err` unions. It costs a decision procedure that is genuinely subtle.

The four features that had to be in from the start, because each is load-bearing and
retrofitting any of them means rewriting the decision procedure: arrows with contravariant
parameters, full negation, μ-types, and atom singletons. All four are in.

## Why it is one BDD per kind

Worth stating because it dictates everything else.

The previous implementation put **every kind of atom in one BDD** — primitives, records,
arrows, tuples, type variables. The consequence was `is_path_satisfiable`: the algorithm
carried a *path* of atom assumptions down the tree and, at every `Any` leaf, re-bucketed
that whole path by kind to decide satisfiability. Two things follow:

1. **It cannot memoise.** Its own comment said so. That is why the checker was exponential.
2. **It cannot do recursion.** Deciding `mu type A = :ok | List[A]` requires assuming a
   recursive query and looking for a contradiction; there was nowhere to put the assumption.

Both are one root cause: mixing kinds forces path-sensitivity, path-sensitivity forbids
memoisation, and without memoisation there is no fixpoint to be coinductive about.

So a type is not one BDD. It is `TyData` (`types.rs`), one field per kind:

    pub struct TyData {
        pub base:    BaseSet,    // i64, f64, str, bool, null, undef — a u8 bitmask
        pub atoms:   AtomSetId,  // :ok, :err — a finite-or-cofinite name set
        pub vars:    AtomSetId,  // rigid type variables, same machinery as atoms
        pub records: BddId,
        pub tuples:  BddId,
        pub arrows:  BddId,
    }

(The old name in this document was `Descriptor`. It is `TyData`, and a `TyId` is an index
into one arena's `Vec<TyData>`.)

Union, intersection and negation are **field-wise**. Emptiness is **every field empty**,
decided independently. An `i64` is never a record, so the kinds never interact and no path
has to be carried anywhere. Each kind's emptiness depends only on that kind's atoms, so it
memoises on the node. That is the whole fix.

`base` is a bitmask because the primitives are a fixed finite set. It carries a sixth bit,
`B_UNDEF`, which is not a value and unwritable in source: field-wise record decomposition
needs a total map from label to type, so "absent" has to be a member of the field lattice
and complement has to be taken within a universe that includes it.

`atoms` and `vars` are finite-or-cofinite (`{:ok, :err}`, or "everything except `{:ok}`"),
because names are countably infinite but any one type mentions finitely many. A negated
name set is never empty, which is why `empty.rs` can treat a non-empty atom component as
proof of inhabitation without looking at the names.

### Emptiness, per kind

`empty.rs`, and its module doc is the current statement.

- **base**: mask == 0.
- **atoms / vars**: the set is empty, i.e. positive with no names.
- **records / tuples**: per BDD cube, meet the positives componentwise, then ask whether
  the negatives cover the result. Escaping a negative means differing on *some* component,
  and the choice has to be searched.
- **arrows**: the hard one.

      ⋀_{i∈P}(sᵢ→tᵢ)  ≤  (s→t)
      iff  ∀ P' ⊆ P:  s ≤ ⋁_{i∈P'} sᵢ   or   ⋀_{i∈P∖P'} tᵢ ≤ t

  Exponential in the number of positive arrows in one intersection, which in real programs
  is one or two. Frisch, Castagna, Benzaken, *Semantic subtyping* (JACM 2008), §4.

### Recursion is coinductive

A recursive type is built by reserving an id before its body exists (`reserve`/`define`),
so the body can name it and the graph simply has a cycle. Equi-recursive: `A` and its
unfolding are the same type. No fold/unfold, no tag, no allocation.

Emptiness carries an **assumption set**. Re-entering a query already in progress returns
"empty" and continues; if the derivation completes without contradiction the assumption was
consistent. What makes that sound rather than wishful is `Solver::tainted`: a result reached
under an assumption never enters the memo, so an answer true only relative to a guess cannot
be replayed as unconditional. **Contractivity is what makes it terminate**, and it is
checked at the declaration (`env.rs`), not here.

The cycle also has to be survived by everything else that walks a type. `Types::substitute`
did not: `record Node { next: Node | null }` passed to any generic function walked the graph
as a tree and overflowed the compiler's stack. It now carries its own guard (`Progress`, in
`types.rs`) that reserves an id on re-entry and closes the cycle, plus a memo so a shared
subtype is rebuilt once. The same fix also stopped it losing the *cofinite* variable
component: `σ(any)` had not been `any`.

### Hash-consing

Names, atom sets, `TyData` and the three shape atoms are all interned. Equal ids denote
equal sets — but **not conversely**: a type reached through a μ unfolding or a deferred
boolean op can be a second id for the same set. So `==` on `TyId` is a fast path, and the
real question is `Solver::is_equiv`, which is `is_subtype` both ways.

## Nominal types have no machinery of their own

A nominal record is an **ordinary record** carrying a reserved `#nominal` field whose type
is the atom of its name; generic arguments ride along as `#0`, `#1`, .... `#` is not an
identifier character, so those labels cannot collide with source.

The payoff: nominal disjointness, nominal-satisfies-structural and generic covariance are
not three rules to keep consistent. They are all the field-wise record decomposition, which
was going to be written anyway.

**Does not hold — nominal identity is a bare name.** `env.rs::record_body` interns
`t.name(&r.name)`, the bare identifier, so two modules each declaring `record Secret`
declared the **same type**, and every claim about nominal distinctness was scoped to a name
rather than a declaration. **Fixed** — identity is qualified (`identity.md`), the `#nominal`
tag IS the declaration key, and an impl for each module's `Secret` coexists and dispatches
correctly. Pinned, deliberately unlisted, as
`tests/lang/types/a_nominal_name_is_not_a_module_identity.neon`.

**Emptiness is still not module-relative — opacity is enforced at the flows.** This
document used to say `opaque` made expansion module-scoped, so the same query could
legitimately answer differently in two modules. It does not: there is one global arena
with no notion of a module, and `empty.rs` stays context-free and memoised.

`opaque` is enforced in `check.rs`, in two halves. The syntactic doors
(`check_opaque_name` / `check_opaque_path`, rule in `env::opacity_permits`) reject
reading a field, building a literal, updating via spread and destructuring outside the
declaring module, its descendants and its immediate parent. The *type-directed* doors
— a structural annotation, argument, return, impl target, cast or `is` test, all true
by nominal-satisfies-structural — are closed by **sealing** (`Types::seal`): erase the
foreign record's contents, re-ask the subtyping at the flow, and reject if it only
held through the contents. The module-relative part lives entirely at the gate; the
solver never sees it. Routes, mechanism and the (small) residue — notably `any`
laundering — are enumerated in `opacity.md`, and each route is a corpus file under
`tests/lang/records/opaque_*`.

## There is no `Erased`, and no way to write one

`any` is **⊤** — inhabited by every value. It is not an erasure marker, and the type
language has no way to say "I do not know".

The previous implementation conflated the two: `any` parsed to `TypeSpecKind::Erased`,
which became `Type::Erased`. Once "the top type" and "I could not work it out" are the same
value, every unknown silently becomes `any`, and ~70 of its ~108 `Erased` constructions were
exactly that — a fallback rather than a decision. There is no `Erased` anywhere in
`compiler/src` today; the only remaining occurrences are these post-mortems.

Structurally: `TyData` has no `Erased` variant, `any` is every kind full, and when the
checker cannot determine a type it **emits a diagnostic**. There is nothing to fall back
*to*, so no fallback can be written.

Erasure is a **lowering** concern. A value of ⊤ needs a uniform runtime representation;
that is a consequence of ⊤, not its meaning, and it is decided in `ir/repr.rs`. Whether
`any` should be allowed to *hold* a container at all is still open — `let a: any = [1,2,3]`
works today, and the answer also decides `List[any]` and `Map[str, any]`. Still open, and
still a language decision rather than a defect.

### The poison is a rigid variable, not a variant

    Env::error_ty()      // a rigid variable named `#error`

Recovery only. It is produced **only** where a diagnostic has already been emitted, and
callers check `Env::is_error` before complaining, so one bad expression yields one error
rather than twenty and checking continues through the rest of the file.

It is a variable under a name source cannot write, so it is disjoint from every nameable
type: `error <: T` and `T <: error` are both false. It is *not* outside the lattice —
`error <: any` still holds — so the force of the poison is `is_error`, not the subtype
relation. Typing an error expression `never` would be actively worse: `never <: T` for
every `T`, so every downstream check would vacuously succeed and the cascade would be
silent instead of noisy.

Poison never reaches lowering, because a failed check does not lower.

**Weaker than documented.** This file used to claim a test asserts no poison survives a
successful check and that the only route to ⊤ is a written `any`. The guards that exist
are `compiler/tests/ir_lower.rs`'s `no_type_variable_survives_lowering` and
`any_never_appears_unless_the_source_type_is_any`, and both are aimed at a program the
pipeline never builds: they lower with `libs = &[]`, use a different entry point than
`cli/src/frontend.rs`, and scan only `f.values()`. Neither test exists under those names any
more, so this needs re-deriving rather than re-citing. Rebuilt correctly the
answer is still 0, so this is latent rather than live — but the assertion is not currently
carrying the weight the paragraph above puts on it.

## Type variables and generics

`fn f[T](x: T) -> T` is checked **once, with `T` opaque** — not only at call sites, or a
generic body's errors surface at every caller instead of at the definition. `vars` is that
opacity: a rigid variable is a singleton disjoint from everything, using the same
finite-or-cofinite machinery as atoms rather than a sixth BDD.

A bound (`where T: Display`) is a constraint checked at the instantiation, not baked into
the atom. Generic arguments are **covariant** (`../decisions.md`) — sound because
collections are values. Inference is structural matching (`generic.rs`), not Castagna's
subtype inference: a variable binds to the first concrete type it meets and stays there, so
`push(xs: List[i64], "s")` is a mismatch rather than a silent widening. Widening is
explicit — a turbofish, or the expected type applied first.

Full polymorphic set-theoretic types (Castagna & Xu 2011: variables under
union/intersection/negation with a semantic notion of instantiation) are a non-goal.
Generics here are parametric, checked with opaque variables, and monomorphised per call
site.

**Was open, now fixed:** the solver is first-wins and returned what it managed, so an
unsolved parameter reached codegen as an ICE. It is a diagnostic now — "cannot infer the type
parameter `T` of `new`: no argument or expected type pins it" — naming both ways out.

## Arrows carry their error type

    ArrowAtom { params: Vec<TyId>, throws: TyId, ret: TyId }

`throws` is covariant, like the return: a function that throws less can stand where one
throwing more is expected.

**`never` is the sound default for an absent `throws`.** A function that throws nothing has
`never` there, and that has to be the resolution of a missing clause. The tempting
alternative — ⊤ — both erases the error path and, because everything is a subtype of ⊤,
switches off the check that a thrown value is an error at all. `main`'s implicit channel is
not a type for the same reason: it is a *rule* (`Throws::ImplicitError` in `check.rs`),
whatever escapes must implement `Error`, checked per throw site.

## Bidirectional: `expected` flows down

Not a nicety — the system does not work without it.

    let nested: Json = [[1.0], ["a"]]

Bottom-up the inner literals are `List[f64]` and `List[str]`. Only the *expected* type tells
them they are `List[Json]`. Covariance makes that a subtype question rather than an equality
one, but the expected type still has to reach the literal for there to be a question. The
same mechanism is why `let x: u8 = 999` must be rejected at the literal.

So every check is `expr(e, expected: Option<TyId>) -> TyId`, and `expected` threads through
branches, arms, arguments and elements. A generic direct call checks each argument
**twice** — once while solving the type parameters, then again under the solution, which is
what lets an expected type reach a lambda argument — and relies on `check_all`'s
deduplication to keep that from doubling diagnostics.

## Narrowing, and the module that is not connected to it

Narrowing is a *set* operation. `match s { is Circle => ... }` binds `s ∧ Circle` in the
arm and leaves `s ∧ ¬Circle` for the fallthrough.

Exhaustiveness falls out: the match covers `s` iff `s ∧ ¬(⋁ arms)` is empty, and the
residual of that subtraction is a **type** naming exactly what was missed — which is what
`print.rs` exists to render. Redundancy is the same query, and catches both "an earlier arm
already took it" and "the subject was never that", because those are one fact.

**The union is over the *exact* arms only, and getting this wrong makes the check
worthless.** An arm is exact when matching it proves the value is *every* value of its
type: `is T`, `:ok`, `null`, `_`. A literal is not — `match n { 1 => ... }` has an arm of
type `i64` but matches one `i64`, and counting it as covering `i64` reports the match
exhaustive with every other integer unhandled. An inexact arm contributes `never` to
coverage and subtracts nothing. A **guard** makes any arm inexact, for the same reason: it
can always decline. Atoms and `null` are exact because they are genuine singletons, which is
the same property that makes `:ok | :err` exhaustiveness work and why `../decisions.md`
refuses to widen an atom to its carrier type.

`bool` needs a special case on top: it is one base bit rather than `:true | :false`, so a
`true` arm's type is the whole of `bool` and subtracting it would exhaust the type after one
arm. The two literals are tracked by hand and `bool` is subtracted only once both have been
seen unguarded.

### Where this is actually implemented

**Held, then did not, and holds again.** `narrow.rs`'s refinement API had zero callers when
this was written; `check.rs` now goes through it in eleven places, including the field-path
refinements added 2026-08-03.

The module encoding the soundness argument below — `Refined` with no `then_ty` to read on
the impossible case, `Projected` with no `never`, 52 unit tests, a long module doc — is
only half connected. `check.rs` uses `narrow::Test`, `project_field` and `project_elem`.
It does **not** call `narrow`, `narrow_is`, `narrow_null`, `narrow_not_null`,
`narrow_atom`, `record_test`, `residual`, `is_exhaustive` or `redundant_arms`. Nothing
outside `narrow.rs` mentions `Refined` except a comment.

What runs instead:

- `check.rs::match_expr` reimplements narrowing inline with a raw `intersect` against a
  running `remaining` residual. It gets the *result* right, including the empty-arm
  diagnostic and exhaustiveness, and its own doc comment explains why.
- **`if` and `while` do not narrow at all.** `if x is str { x }` on `x: i64 | str` is a
  diagnostic — `expected str, found i64 | str`. Verified 2026-07-19.
- Redundant-arm reporting does not happen: `redundant_arms` is written, tested, and
  uncalled.

A green suite over a disconnected module reads exactly like a green suite over a connected
one. That is the point, and it is why the reconnection above is worth checking rather than
assuming.

### The trap: an empty branch is a diagnostic, never dead code

Inside `fn show[T](x: T)`, `T` is opaque. That has a sharp edge, and it is **soundness**
rather than a limitation:

    fn g(s: str) -> str { s }
    fn f[T](x: T) -> str {
        if x is i64 { g(x) } else { "no" }   // x : T ∧ i64 = never
    }

`T ∧ i64` is empty, so a narrowing implementation binds `x: never` — and `never <: str`, so
`g(x)` typechecks **vacuously**. Then `f(5)` instantiates `T := i64`, the branch is live,
and `g` receives an `i64` through a `str` slot. This is the error-recovery trap entered by a
different door: a type meaning "cannot happen" satisfies everything downstream, so the
cascade is silent rather than noisy.

**The rule: a test the subject could never satisfy is reported, and no arm is ever handed
an empty binding.** In `match_expr` both halves are implemented: the impossible test is a
`Mismatch` against the *subject* (not against `remaining`, since an exhausted `remaining`
is the ordinary trailing-`_` case), and a binding that comes out empty is replaced with
poison rather than `never`. Pinned by
`tests/lang/match/impossible_arm_on_a_type_parameter.neon`.

In `if`, the same program is rejected — but *incidentally*, because `if` does not narrow,
so `x` stays `T` and `g(x)` fails on `T` not being `str`. Right answer, wrong reason; it
will stop being the right answer the day `if` learns to narrow, which is precisely why
that work has to go through `narrow.rs` rather than around it.

So a type parameter cannot be narrowed, **and cannot be pretended to have been narrowed
either.** Polymorphic semantic subtyping is the real answer and is not this version.

## `as` is a reinterpretation, not a checked cast

`check.rs`'s `As` arm gates on one question: is `from ∧ to` inhabited, or does a newtype
boundary bridge the two (`m as f64` for `newtype Meter = f64` is exactly what a newtype is
for, and the two are disjoint). If not, `ImpossibleCast`.

That is the only check. The assertion is **never discharged at runtime**. Verified
2026-07-19: `let n: str | null = null; n as str` compiles, runs, and yields `""`.
`(x: i64|str) as str` on an `i64` reads garbage.

The checker is arguably right not to reject these — `as` exists to assert what the checker
cannot prove — but it is a reinterpret cast wearing a checked cast's name. Making it trap
is a language decision with a cost on every narrowing, and it is unmade.

## Protocols are bounds, never types

A protocol name is not a type and cannot appear in a type position. It constrains a
parameter (`where T: Display`) or names a set of impls to dispatch through. The reason is
the one above: inside a generic body `T` is opaque, so there is no `impl P for T` to find
and there never will be — the call resolves against the **bound in scope**, and the bound
is discharged at the call site once `T` is a real type.

Everything else about the subsystem is `dispatch.md`. The shape that matters here: the
checker records its choice and **nothing downstream re-resolves**. That is the same fix as
the `expr_types` keystone — record the decision at the point it is made — applied to what
was a last-write-wins `method_to_protocol` map.

**Not checked:** a protocol method's *default body*. `check.rs`'s `decls` calls
`fn_body(module, m, &[])` for it, so the protocol's subject is unbound and any mention of
`T` in the body is `unknown type T`. **Both halves are now false**, and were separately: the
checker binds the subject as a rigid variable when it checks a default body, and as of
2026-08-08 the impl inherits the method too, so the body is reached at run time. Verified by
`protocols/default_body_runs.neon`, including a default body whose inner call lands on an
impl's override. `dispatch.rs`'s
`result_of` carries a fallback for an impl relying on a default, and its doc records that
the path is currently unreachable for exactly this reason.

## The keystone: `TypecheckResult` carries per-expression types

`result.rs`, and its module doc is the current list. The rule it enforces is
one-directional: **lowering asks, it never derives.**

    expr_types      ExprId -> TyId      <- this one
    resolved_calls  ExprId -> Resolution
    caught_types    ExprId -> TyId      the union a `try` can catch
    generic_args    ExprId -> [(name, TyId)]
    declared_types  ExprId -> TyId      a `let`'s annotation, keyed on its initialiser
    tested_types    ExprId -> TyId      the type an `is` asks about, resolved
    resolved_lambdas                    written, read by nothing

The previous implementation kept only the resolutions and **threw every expression type
away**. IR lowering had to re-derive them, which is why `infer.rs` existed; it could not
always succeed, so it fell back to `Erased`; that leaked into `NeonValue` boxing, which
invented vtables, which produced `*_Any` collections with 24-byte slots that `push` read as
8 — an ASan stack-buffer-overflow on every `list::new()`.

One discarded hashmap, four subsystems of consequences. It is still the single most
important line here.

Keying is `ExprId`, assigned at parse time and unique across the whole compilation
(`ast::number_exprs_from`), so one result covers every module including the stdlib. The old
`span.start` key was fragile; a *collapsing* key is the same failure with a longer fuse, and
the class is worth watching. The instance that was live in the checker — an interpolation
hole's `to_string` resolution written to the hole expression's own `ExprId`, overwriting that
expression's own call resolution — is **fixed**: the hole's resolution lives in its own table
(`interp_call`), and `"#{area(q)}"` on a `Sq { side: 3 }` prints `9`. Two more of the class
turned up later and are also fixed: `derive`'s generated impls sharing the record's span, and
`collect_impl_bodies` keying bodies by `protocol$head$method`.

## One error channel

`check_all` returns every diagnostic. Read the return value and you have seen everything.

This used to be two channels — the return value, and `Env::errors`, where resolving a type
annotation raises — and a caller reading one silently dropped the other's. `let x:
NoSuchType = 5` compiled, and the poison it produced reached codegen. There were 23 call
sites and every one had to remember. `check_all` now drains `Env::errors` into its result,
so `env.errors()` is empty afterwards and a caller cannot double-report either. The list is
sorted by span so the two phases interleave in source order, and deduplicated on
(span, kind).

Callers that gate on *declarations* — read `env.errors()` after `build_with`, refuse to
check bodies against signatures that did not resolve — are unaffected, because they read
that list before this runs.

**Was wrong, now fixed:** a `TypeError` had no file id, so a stdlib diagnostic rendered
against the user's file at a fabricated location. It carries `module` now, and a stdlib
mistake is rendered against the stdlib source.

## Module layout

    typecheck/
      bdd.rs       the shared BDD, one arena per shape kind
      types.rs     TyId, TyData, atoms, hash-consing, union/intersect/negate, substitute
      empty.rs     Solver: the emptiness procedure, per kind, with the assume set.
                   `is_subtype` and `is_equiv` live here — there is no subtype.rs
      env.rs       records, aliases, protocols, impls, coherence; contractivity; the poison
      resolve.rs   ast::TypeSpec -> TyId, and Scope (module + rigid variables)
      generic.rs   structural matching to solve a generic call's arguments
      narrow.rs    refinement, projection, exhaustiveness — the refinement half uncalled
      ordered.rs   does a type have a structural order (`<`, `marker Ord`)
      dispatch.rs  protocol resolution — see dispatch.md
      print.rs     TyId -> readable Neon syntax, so a residual can be a diagnostic
      check.rs     the checker: a type for every expression
      result.rs    TypecheckResult

## The checker layer is where the soundness holes were

The solver answers subtyping questions correctly; the layer above asked the wrong ones. All
of these were **accepted** by the previous implementation and are rejected now:

    let x: u8 = 999          // literal out of range
    let y: i64 = 1 + 2.5     // no implicit numeric promotion
    -"hi"                    // operator typing
    p.field                  // field access on a partial union
    !5                       // `not` on a non-bool

None were solver bugs. They were the checker not checking, and that is still where to look
first — the six wrong-answer defects found on 2026-08-03/04 were all in that layer too, not
in the solver.

## Non-goals

- Mutual recursion between μ-aliases (a clear "not yet supported" error; pinned by
  `tests/lang/types/mu_type_mutual_recursion_unsupported.neon`).
- Type inference beyond local propagation of an expected type. Signatures are explicit.
- Polymorphic set-theoretic types in their full generality.

## Risks

- **Arrows are where this gets hard.** The decomposition is exponential in the number of
  positive arrows in one intersection. Fine in practice, but the first place to look when
  something hangs.
- **Coinduction is easy to get subtly wrong.** Assuming the wrong polarity gives an unsound
  "yes". Every recursive test must be checked in *both* directions. The `tainted` flag is
  the load-bearing part; it is one boolean between correct and a memoised guess.
- **A cyclic type graph is a hazard for every walk over it, not just for `is_empty`.**
  `substitute` proved that by overflowing the stack on an ordinary `record Node`. Anything
  new that recurses over `TyData` needs its own guard, and there is no shared one to
  inherit.
- **Covariance plus `expected` propagation may hide inference gaps.** Covariance makes many
  checks succeed that invariance would have rejected, so a missing `expected` thread shows
  up later and further away than it otherwise would.
- **A green test suite over a disconnected module is indistinguishable from a green suite
  over a connected one.** `narrow.rs` is the standing example.
