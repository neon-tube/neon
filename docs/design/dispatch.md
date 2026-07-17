# Design: protocol dispatch

**Status:** proposed. Companion to `typechecker.md`.

This is where the previous implementation's erasure disaster started. `ir/lower.rs:1270`:

    let ret_ty = if method_name == "eq" { Type::Erased } else { ... };

Every protocol call except `eq` returned `Erased` — every `<`, every `cmp`, every
`to_string`, every user method, round-tripping through 24-byte `NeonValue`. That is not a
bug you patch. It is what happens when you have no answer to *what does a dispatched call
return*, so the answer has to fall out of the design rather than be bolted on.

## Two resolution paths, and they are different

The receiver is either a concrete type or a type variable, and those are not the same
question.

**Concrete** — `len("hi")`. Find the impls.

**A type variable** — inside a generic body:

    fn show[T](x: T) -> str where T: Display {
        to_string(x)          // T is opaque. NO impl applies. Ever.
    }

The body is checked **once**, with `T` opaque, so there is no `impl Display for T` to find
and there never will be. `to_string(x)` resolves against the **bound in scope**, not
against the impl registry. At the call site `show(5)`, `T := i64`, and *then* the bound is
discharged by finding a real `impl Display for i64`.

So: bound-directed inside, impl-directed outside. Conflating them is how you end up
checking generic bodies at every call site and reporting a library's errors to its users.

A constructor variable takes the same path. In

    fn count[C[_], T](c: C[T]) -> i64 where C: Container { size(c) }

`size(c)` resolves via the `C: Container` bound; at `count(a_box)`, `C := Box` and the
bound is discharged against `impl Container for Box`. Nothing new — the variable happens
to be a constructor, and neither side of the path cares.

## The algorithm, for a concrete receiver

    1. Lexical first.
       A local or module fn named `m` shadows protocols entirely. (Pinned:
       protocols/local_name_shadows_protocol_method.neon.)

    2. Candidates.
       Every impl of every protocol declaring `m` at this arity, from every module
       in scope.

    3. Dispatch position.
       The first parameter whose declared type is the protocol's subject. If none —
       `fn make() -> T` — the EXPECTED type. (See "receiverless" below.)

    4. Applicability.  S = the type at the dispatch position.
       Applicable = { impl | S ∧ targetᵢ ≠ ∅ }
       An emptiness query per candidate, not a name match. This is step 3 of the old
       four-step resolution, done properly.

    5. Coverage.
       S <: ⋁ targetᵢ
       else: "no impl of P for `S ∧ ¬⋁targetᵢ`" — and that difference is a *type*, so
       the diagnostic names exactly the part with no impl. A nominal system cannot say
       this.

    6. Specificity, then shape.
       Discard any impl strictly less specific than another that also covers the same
       values (decisions.md: nested overlap only, so the applicable set is a chain per
       value).
       |Applicable| == 1 and S <: target  → a direct call.
       otherwise                          → a switch on the runtime tag, with a direct
                                            call per arm. Not a vtable: the applicable
                                            set is known here.

    7. Return type = ⋁ retᵢ over Applicable.

**Step 7 is the whole document.** If the impls agree, that is the concrete type and the
call is as precise as a direct one. If they disagree, it is a union — *exactly* as
imprecise as the receiver is, and no more. There is nowhere for `Erased` to enter, because
there is no case where the answer is unknown.

## Ambiguity across protocols

Two protocols may both declare `go`. With impls of both for `R`, `go(r)` is ambiguous —
not because of overlap within a protocol (that is a coherence question, settled in
decisions.md) but because two *different* protocols both answer.

That is an error naming both, and it is resolved by qualifying:

    A::go(r)
    B::go(r)

Qualification skips steps 1–2 and fixes the protocol; the rest is unchanged. (Pinned:
protocols/ambiguous_call.neon, ambiguity_resolved_by_qualification.neon.)

## Generic impls

    impl[T] Sized for List[T]

The target has free variables, so applicability is **matching**, not just an emptiness
query: find `T` such that `S <: List[T]`.

Covariance does real work here. For `S = List[i64] | List[str]`, there is no single `T`
matching each arm separately — but `List[i64] | List[str] <: List[i64 | str]`, so
`T := i64 | str` and one instantiation covers it. Under invariance this would have needed
two instantiations of one impl and a runtime switch between them, which is absurd. Take
the least such `T`.

## Higher-kinded impls

    protocol Container for C[_] {
        fn size[T](c: C[T]) -> i64
    }
    impl Container for Box            // the CONSTRUCTOR, not Box[T]

The subject is a constructor of a declared arity, and the impl target is a bare
constructor name. Applicability is a constructor match: `S = Box[i64]` has head `Box`,
which is the target. The method's own `[T]` is separate from the protocol's `C`.
(Pinned: protocols/generic_impl.neon.)

## Receiverless methods

    fn make() -> T                      // no parameter mentions the subject
    let xs: List[i64] = new()           // dispatch on the expected type

The expected type must reach the call, which the bidirectional design already requires.
A turbofish overrides: `new[i64]()`.

**This is exactly what the previous implementation got wrong**, and it is worth naming
because the failure was so far from the cause. `@native fn new[T]() -> List[T]` inferred
`T` only from the return type; lowering could not propagate it; it fell back to `Erased`;
that produced `List_Any` with 24-byte `NeonValue` slots, which `push` then read as 8-byte
— an ASan stack-buffer-overflow on **every `list::new()`**. A dispatch decision became a
memory-safety bug four subsystems away.

## Recording the decision

    resolved_calls: HashMap<ExprId, Resolution>

    enum Resolution {
        Direct { impl_id: ImplId, subst: Subst },
        Switch { arms: Vec<(TypeId, ImplId)>, subst: Subst },
        Bound  { param: String, protocol: ProtocolId },   // inside a generic body
    }

The checker decides; **nothing downstream re-resolves**. The previous implementation kept
a `method_to_protocol` map that was last-write-wins — the same class of bug as discarding
per-expression types, and the same fix: record the decision where it is made.

## Known limitation: binary methods

    protocol Eq for T { fn eq(a: T, b: T) -> bool }
    eq(s1, s2)                          // both s1, s2 : Shape

Dispatch picks on `s1`. In the `Circle` arm the chosen impl wants `eq(a: Circle, b: Circle)`
— but `s2` is still `Shape`, so it is a type error at the argument, not at the dispatch.

This is the binary-method problem and every language has it: Java's `equals` takes
`Object`, Rust has `PartialEq<Rhs>`. The answer here is to write `impl Eq for Shape` and
match inside, which is the honest thing anyway — deciding what two arbitrary shapes'
equality means is a real decision, not something dispatch should guess.

Dispatching on the *tuple* of subject parameters would work and is a 2-D switch. Not for
v1.

## Bounded impls

    impl[T] Display for List[T] where T: Display

Applicability now has a side condition. `List[Circle]` matches the target with
`T := Circle`, but the impl only applies if `Circle: Display` also holds — so discharge is
a recursive search, not a lookup.

    discharge(S, P, depth):
        if depth > MAX: error "bound too deep"
        if (S, P) in assuming: return Ok        // coinductive: cycles succeed
        assuming += (S, P)
        find impls of P applicable to S (step 4)
        for each: discharge every `where` bound under the match's subst

The cycle check is not optional. `impl Display for List[T] where List[T]: Display` is
accepted by every rule above and loops forever without it. Assuming-success on re-entry is
the same trick `empty.rs` uses for μ-types, for the same reason: the recursion is
productive, and the fixpoint we want is the greatest one.

This is where Rust's trait solver gets slow, and we should expect to pay the same. The
depth cap is what turns a pathological program into a diagnostic instead of a hang.

Without this you cannot print a list, a map, or a nested record without one impl per
element type. It is not optional either.

## Default method bodies

A protocol method may carry a body. For a given impl, each method is **its override if
present, else the protocol's default.** That is the whole rule.

A default is not an impl. It never enters the candidate set, never competes on
specificity, never participates in step 6. `impl Area for Shape` omitting `area` inherits
the default; `impl Area for Circle` overriding it still wins for circles, by ordinary
specificity, because the *impls* are what is ranked — not where each impl's body came
from.

## Coherence: what is enforced, and what cannot be yet

`orphan impl P for T` parses, reaches `ImplDef.orphan`, and clears two of the three
rules in `decisions.md`:

- **Only in the root application.** `Env::build_as(module, Unit::Library)` rejects it.
  A library carrying one imposes its choice on every dependent.
- **Must fill a gap.** `target ∧ ⋁ existing = ∅`, by emptiness query. This is the rule
  that stops the root hijacking a library's `impl Area for Shape` for Circle values
  while the library's own code keeps taking the wide path.

The third — **an orphan must own neither side, and a plain impl must own one** — is
**not implemented and cannot be yet.** Ownership is a property of the *library* a
declaration came from, and `use` does not load a dependency: every declaration `Env`
can see is local. The question therefore has exactly one answer, and asking it would
be theatre. It belongs in `check_coherence` the day `use` resolves a foreign module,
and until then a plain `impl TheirProtocol for TheirType` cannot be caught because
`TheirProtocol` cannot exist.

This is worth stating plainly rather than leaving as a green test suite: the corpus
test `orphan_impl_fills_a_gap` passes today for a *weaker* reason than it will later.
Its protocol is local, so once ownership is checkable it becomes "you own `Area`; drop
`orphan`". The test is right about the rule it names and wrong about the one it does
not.

A related gap: `OrphanOverlaps` names the protocol, not the overlapping values. The
intersection **is** the diagnostic — that is the whole point of the representation —
but printing it needs a `TyId` formatter, which does not exist. Every diagnostic that
wants to name a type is blocked on the same thing.

## Open

The design has an answer for every case the corpus pins. The implementation gaps above
are gaps in `use` and in a type printer, not in the design.
