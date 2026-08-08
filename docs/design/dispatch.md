# Design: protocol dispatch

**Status:** built, with two named holes. Companion to `typechecker.md`; the
implementation is `compiler/src/typecheck/dispatch.rs` plus the coherence checks in
`env.rs`. Its module doc is more current than this file and is not repeated here.

Anything marked **Does not hold** is a property this document asserted and the code does
not have.

## Why the return type is the whole design

This is where the previous implementation's erasure disaster started. `ir/lower.rs:1270`:

    let ret_ty = if method_name == "eq" { Type::Erased } else { ... };

Every protocol call except `eq` returned `Erased` — every `<`, every `cmp`, every
`to_string`, every user method, round-tripping through 24-byte `NeonValue`. That is not a
bug you patch. It is what happens when you have no answer to *what does a dispatched call
return*, so the answer has to fall out of the design rather than be bolted on. Step 7 below
is that answer, and it is why the file exists.

## Two resolution paths, and they are different

The receiver is either a concrete type or a rigid type variable, and those are not the same
question.

**Concrete** — `len("hi")`. Find the impls.

**A type variable** — inside a generic body:

    fn show[T](x: T) -> str where T: Display {
        to_string(x)          // T is opaque. NO impl applies. Ever.
    }

The body is checked **once**, with `T` opaque, so there is no `impl Display for T` to find
and there never will be. `to_string(x)` resolves against the **bound in scope**
(`Resolution::Bound`), and `check.rs` verifies the enclosing signature actually declared it,
following supertraits via `Env::protocol_extends`. At the call site `show(5)`, `T := i64`,
and *then* a real `impl Display for i64` has to exist.

So: bound-directed inside, impl-directed outside. Conflating them is how you end up checking
generic bodies at every call site and reporting a library's errors to its users.

A constructor variable takes the same path. In

    fn count[C[_], T](c: C[T]) -> i64 where C: Container { size(c) }

`size(c)` resolves via the `C: Container` bound; at `count(a_box)`, `C := Box`. Nothing new
— the variable happens to be a constructor, and neither side of the path cares.

### What counts as rigid

`dispatch::rigid_name` returns a name only when the receiver is *exactly one bare variable*
— every other component of the `TyData` empty, not merely the primitive bases.

That qualifier was a live bug. `T | :none` answered `Some("T")` because the atom component
was invisible, so the call resolved to `Resolution::Bound`, lowering could not name an impl
for an abstract receiver, and the program **ran** and printed `<todo: bound: abstract
receiver>`. The same signature written `T | null` set a base bit, fell through to
`applicable`, and was a diagnostic — a wrong answer or an error depending only on which kind
the other arm lived in. Fixed and pinned by
`tests/lang/protocols/a_variable_union_receiver_is_not_rigid.neon`.

The underlying gap is not fixed. A `Resolution::Bound` whose `T` is *instantiated* at a
union still reaches lowering's abstract-receiver path:

    fn show[T](v: T) -> str { "#{v}" }    // at T = A | B

used to compile clean, exit 0, and print `<todo: bound: abstract receiver>`. It needed the
same variant-switch machinery as `Resolution::Switch` below, and got it: both lower to an
if-else chain over the checker's arms, each testing the receiver with the runtime tag test
`is` compiles to, projecting to the arm's type, and calling that arm's impl directly. Pinned
as `protocols/union_receiver_dispatches_by_variant.neon`.

## The algorithm, for a concrete receiver

    1. Lexical first.
       A local shadows everything; then a module fn shadows protocols entirely.
       A module fn fenced off by `internal` says so rather than falling through to
       dispatch and reporting a missing method.
       (Pinned: protocols/local_name_shadows_protocol_method.neon.)

    2. Candidates.
       Every protocol declaring `m`. A bare name may also have been imported as one
       specific protocol's method (`Env::imported_method`), which fixes the protocol
       exactly as qualification does.

    3. Dispatch position.
       The first parameter whose declared type is the protocol's subject. If none —
       `fn make() -> T` — the EXPECTED type. (See "receiverless" below.)

    4. Applicability.  S = the type at the dispatch position.
       Applicable = { impl | S ∧ targetᵢ ≠ ∅ }
       An emptiness query per candidate, not a name match.

    5. Coverage.
       S <: ⋁ targetᵢ
       else: "no impl of P for `S ∧ ¬⋁targetᵢ`" — and that difference is a *type*, so
       the diagnostic names exactly the part with no impl. A nominal system cannot say
       this.

    6. Specificity, then shape.
       Discard any impl strictly less specific than another that also applies
       (`most_specific`).
       |Applicable| == 1 and S <: target  → Resolution::Direct.
       otherwise                          → Resolution::Switch: a switch on the runtime
                                            tag with a direct call per arm. Not a vtable:
                                            the applicable set is known right here.

    7. Return type = ⋁ retᵢ over Applicable.  Likewise `throws`.

**Step 7 is the point.** If the impls agree, that is the concrete type and the call is as
precise as a direct one. If they disagree, it is a union — *exactly* as imprecise as the
receiver is, and no more. There is nowhere for `Erased` to enter, because there is no case
where the answer is unknown.

An impl that does not carry the method is one relying on the protocol's **default body**, so
the protocol's own signature answers for it. Contributing nothing instead would take the
union over an empty set, which is `never` — a type no value inhabits, handed to lowering as
a call's result with no diagnostic anywhere. That case is currently unreachable only because
a protocol method with a body does not typecheck at all (see "default method bodies"), so
the silent `never` was one fix away from being live.

### Step 6 produces a resolution lowering cannot lower

**Does not hold — `Resolution::Switch` is unimplemented.** `ir/lower.rs:1695` answers it
with `unhandled_note("dispatch switch")`, which emits a **string constant**. Verified
2026-07-19: two records, one impl each, a receiver typed at their union — the program
compiles, links, exits 0, and prints

    <todo: dispatch switch>

The checker's half is correct and the marker is unmistakable, but it is program output, not
a diagnostic. This and the abstract-receiver case above are the same missing feature: a
runtime tag switch with a per-variant call.

## Ambiguity across protocols

Two protocols may both declare `go`. With impls of both for `R`, `go(r)` is ambiguous — not
because of overlap within a protocol (a coherence question, settled in `../decisions.md`)
but because two *different* protocols both answer. That is an error naming both, resolved by
qualifying:

    A::go(r)
    B::go(r)

Qualification fixes the protocol and skips the search; the rest is unchanged. (Pinned:
protocols/ambiguous_call.neon, ambiguity_resolved_by_qualification.neon.)

One detail this document did not record: **a candidate with no impls at all does not make a
call ambiguous.** If several protocols declare `go` but only one has any impl, that one is
chosen silently. Undocumented — no reasoning for it was found in the code or the corpus, and
it is a plausible source of a surprising resolution the day a second impl appears.

## Higher-kinded impls

    protocol Container for C[_] {
        fn size[T](c: C[T]) -> i64
    }
    impl Container for Box            // the CONSTRUCTOR, not Box[T]

The subject is a constructor of a declared arity and the impl target is a bare constructor
name, so an `ImplDef` carries `target_head: Some("Box")` and no `target` — it is not a type
until it is applied. This is a separate path in `resolve` (`hkt_resolve`): applicability is a
head match, `Box[i64]` has head `Box`.

The method's own `[T]` is separate from the protocol's `C` and is instantiated from **every
argument**, not from the receiver alone — `fold`'s accumulator comes from `init`, not from
the container. (Pinned: protocols/generic_impl.neon.)

Head-only impls are skipped by the coherence and supertrait checks below, which work in
`TyId`s and have no type to work with here.

## Generic impls

**Does not hold — not implemented.**

    impl[T] Sized for List[T]

This document described applicability for such an impl as *matching* rather than an
emptiness query: find `T` with `S <: List[T]`, taking the least such `T`, with covariance
doing real work (`List[i64] | List[str] <: List[i64 | str]`, so one instantiation covers a
union that no per-arm match would).

`applicable` does no matching. It intersects the receiver with the target as written, and
the target's `T` resolved to a **rigid** variable, which is disjoint from every concrete
type. So the intersection is empty and the impl never applies. Verified 2026-07-19:

    record Pair[T] { a: T, b: T }
    impl[T] Tag for Pair[T] { ... }
    tag(Pair { a: 1, b: 2 })
    // Error: cannot call `tag`: no impl of `Tag` for `Pair[i64]`

The parser and `ImplDef` accept and store `generics`; nothing consumes them. A generic
container is implementable only through the higher-kinded form above, for a protocol whose
subject is a constructor. The reasoning in this section is still the design — it is simply
not the code.

## Bounded impls

**Does not hold — not implemented, and not representable.**

    impl[T] Display for List[T] where T: Display

`ast::ImplDecl` has no `wheres` field and `parser::impl_decl` does not accept a `where`
clause after an impl target, so this does not parse. `ImplDef` therefore has no side
condition to discharge and there is no discharge search.

The design, kept because it is the part a reader cannot reconstruct: applicability would
gain a side condition — `List[Circle]` matches with `T := Circle`, but the impl applies only
if `Circle: Display` also holds — so discharge is a recursive search, not a lookup:

    discharge(S, P, depth):
        if depth > MAX: error "bound too deep"
        if (S, P) in assuming: return Ok        // coinductive: cycles succeed
        assuming += (S, P)
        find impls of P applicable to S (step 4)
        for each: discharge every `where` bound under the match's subst

The cycle check is not optional: `impl Display for List[T] where List[T]: Display` is
accepted by every rule above and loops forever without it. Assuming success on re-entry is
the same trick `empty.rs` uses for μ-types, for the same reason — the recursion is
productive and the fixpoint wanted is the greatest one. This is where Rust's trait solver
gets slow and we should expect to pay the same; the depth cap turns a pathological program
into a diagnostic instead of a hang.

Without it you cannot print a list, a map, or a nested record without one impl per element
type. What exists instead is `to_string` in the stdlib and the built-in `Ord`/`Eq` marker
rules (`ordered.rs`), which answer structurally rather than by impl search.

What *is* implemented is the neighbouring feature, **protocol supertraits**:
`protocol Ord for T where T: Eq` is stored, `Env::check_supertraits` requires the super's
impls to cover every target of the sub's, and `protocol_extends` makes a `where T: Ord`
bound satisfy a call needing `T: Eq`. (Pinned: protocols/protocol_with_supertrait.neon.)

## Receiverless methods

    fn make() -> T                      // no parameter mentions the subject
    let xs: List[i64] = new()           // dispatch on the expected type

Implemented, and verified. The expected type must reach the call, which the bidirectional
design already requires; `NoReceiver` is the diagnostic when it does not.

**This is exactly what the previous implementation got wrong**, and it is worth naming
because the failure was so far from the cause. `@native fn new[T]() -> List[T]` inferred `T`
only from the return type; lowering could not propagate it; it fell back to `Erased`; that
produced `List_Any` with 24-byte `NeonValue` slots which `push` read as 8-byte — an ASan
stack-buffer-overflow on **every `list::new()`**. A dispatch decision became a memory-safety
bug four subsystems away.

## Recording the decision

    resolved_calls: HashMap<ExprId, Resolution>

    pub enum Resolution {
        Direct(ImplId),
        Switch(Vec<(TyId, ImplId)>),                   // (arm type, impl), sorted
        Bound { param: String, protocol: ProtocolId }, // inside a generic body
    }

No `subst` field — this document once showed one. A generic method's substitution is
recomputed in lowering from the argument *reprs* (`match_repr`), and a generic *call*'s
solved arguments are carried separately in `TypecheckResult::generic_args`.

The checker decides; **nothing downstream re-resolves.** The previous implementation kept a
`method_to_protocol` map that was last-write-wins — the same class of bug as discarding
per-expression types, and the same fix: record the decision where it is made.

**Stale, now fixed.** An interpolation hole's `to_string` resolution used to be written to
the hole expression's own `ExprId`, so `"#{area(q)}"` destroyed `area(q)`'s own resolution
and miscompiled. The hole's resolution lives in its own table (`interp_call`) now, so the two
can never collide; `"#{area(q)}"` on a `Sq { side: 3 }` prints `9`.

## Default method bodies

The rule: for a given impl, each method is **its override if present, else the protocol's
default.** `Env::check_impl_completeness` enforces the other half — an impl must supply every
method the protocol declares *without* a body, reported at the impl rather than at the first
call that reaches the hole (pinned: protocols/impl_missing_method.neon).

A default is not an impl. It never enters the candidate set, never competes on specificity,
never participates in step 6. `impl Area for Shape` omitting `area` inherits the default;
`impl Area for Circle` overriding it still wins for circles, by ordinary specificity, because
the *impls* are what is ranked — not where each impl's body came from.

**Does not hold — a default body is never type-checked.** `check.rs`'s `decls` calls
`fn_body(module, m, &[])` for a protocol method with a body, so the subject is unbound and a
signature mentioning `T` reported `unknown type T` at the declaration. **Fixed** — the
subject is bound as a rigid variable when the body is checked, and an impl inherits any
defaulted method it does not write, so step 7's signature fallback is exercised for the first
time (`protocols/default_body_runs.neon`).

## Known limitation: binary methods

    protocol Eq for T { fn eq(a: T, b: T) -> bool }
    eq(s1, s2)                          // both s1, s2 : Shape

Dispatch picks on `s1`. In the `Circle` arm the chosen impl wants `eq(a: Circle, b: Circle)`
— but `s2` is still `Shape`, so it is a type error at the argument, not at the dispatch.

This is the binary-method problem and every language has it: Java's `equals` takes `Object`,
Rust has `PartialEq<Rhs>`. The answer here is to write `impl Eq for Shape` and match inside,
which is the honest thing anyway — deciding what two arbitrary shapes' equality means is a
real decision, not something dispatch should guess.

Dispatching on the *tuple* of subject parameters would work and is a 2-D switch. Not now.

## Coherence: what is enforced, and what cannot be yet

`Env::check_coherence` runs after every impl is resolved. Three checks, in order:

- **An `orphan impl` only in the root application.** `Env::build_as(module, Unit::Library)`
  rejects it: a library carrying one imposes its choice on every dependent.
- **An orphan must fill a gap.** `target ∧ ⋁ existing = ∅`, by emptiness query. This is what
  stops the root hijacking a library's `impl Area for Shape` for `Circle` values while the
  library's own code keeps taking the wide path.
- **Two plain impls may overlap only when nested.** Added 2026-07-19. `most_specific`'s
  correctness rests on the applicable set forming a chain per value, and its own doc asserted
  that rule held while **nothing checked it** for non-orphans: `impl Tag for i64 | str`
  beside `impl Tag for str | bool` was accepted by the checker and failed in the *C
  compiler*, on a mangled-name collision unrelated to the real mistake. (Pinned:
  protocols/two_impls_may_not_overlap_unnested.neon.) Head-only (higher-kinded) impls are
  skipped: they have no `TyId` to intersect.

The third rule from `../decisions.md` — **an orphan must own neither side, and a plain impl
must own one** — is **not implemented and cannot be yet.** Ownership is a property of the
*library* a declaration came from, and `use` does not load a dependency: every declaration
`Env` can see is local. The question has exactly one answer, and asking it would be theatre.
It belongs in `check_coherence` the day `use` resolves a foreign module.

Worth stating plainly rather than leaving as a green test suite: `orphan_impl_fills_a_gap`
passes today for a *weaker* reason than it will later. Its protocol is local, so once
ownership is checkable it becomes "you own `Area`; drop `orphan`". The test is right about
the rule it names and wrong about the one it does not.

A second caveat used to live here: every one of these checks is an emptiness query over a
nominal identity that is a **bare name**, so two modules each declaring `record Secret`
intersect and overlap is computed against a coarser notion of type than the rules assume.
**Fixed** — identity is qualified (`identity.md`), the `#nominal` tag IS the declaration key,
and two modules' `Secret`s are disjoint. An impl for each coexists and dispatches correctly.

**Stale, now fixed:** this document used to say `OrphanOverlaps` could only name the
protocol because printing a type needed a `TyId` formatter that did not exist. `print.rs`
exists, and `OrphanOverlaps`, `ImplOverlaps` and `NoImpl` all render the intersection or
residual itself — which was always the point of the representation.

## The gaps, in one place

Design questions: none open. Every case the corpus pins has an answer here.

Implementation. Items 1-4 below are **done** — they were what the serialization work was
for, and `docs/design/serialization.md` records what each cost:

1. ~~`Resolution::Switch` lowers to a string constant.~~ Both it and `Resolution::Bound` at
   a union instantiation lower to a per-variant switch.
2. ~~`Resolution::Bound` at a union instantiation.~~ It also carries `subject_pos` now, so a
   dispatch whose subject is the RETURN reads its head from the result rather than from
   `args[0]` — which was silently the wrong value.
3. ~~Generic impls (`impl[T] P for List[T]`) never apply.~~ `match_head` treats the impl's
   own generics as holes and matches by unification.
4. ~~Bounded impls do not parse.~~ They parse, and the context is discharged under the
   substitution by `dispatch::satisfies`.

Still open:

5. ~~Default method bodies are unchecked, which also makes step 7's default fallback
   unreachable.~~ Both fixed 2026-08-08.
6. Ownership-based coherence, blocked on `use` resolving a foreign module.
7. `Resolution::Bound` picks an impl by head string with no specificity ordering, so a
   concrete `impl P for List[i64]` under a generic `impl[T] P for List[T]` loses to the
   generic one there. A generic impl also declines a union receiver needing a different
   substitution per variant. Both want the same fix: arms carrying their own substitution.
