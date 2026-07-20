# Decisions

The corpus (`tests/lang/`) says **what** the language does. This says **why**, and what was
rejected.

Where a decision states an intention the compiler does not yet meet, it says so and points
at `TODO.md`, which lists what is known-broken with a repro for each. A decision that
quietly drifts ahead of the code is worse than no decision: this project has spent whole
days chasing bugs whose shape was a comment asserting a property that had stopped being
true, and a reader trusting it and stopping looking.

---

## Types

### Recursive types are an explicit `mu type`

    mu type A = :ok | List[A]

Well-formed iff all of:

1. Recursive references occur **beneath a structural constructor**. `mu type T = T | i64`
   is an error.

   The occurrence must be **in the `mu`'s own body**, guarded by a constructor written
   there: a **generic argument** (`List[A]`, `Map[str, A]`), a **tuple element**, or an
   **arrow return**. `mu type A = :ok | List[A]` guards `A` as List's argument.

   The recursion is **not** discovered by reaching into a separate nominal record's
   fields. `mu type Inner = Rng` where `record Rng { seed: Inner | null }` is **rejected**:
   `Inner` never appears in its own body (`Rng`). The recursion there belongs to `Rng`,
   and a record recurses on its own account without a `mu` — `record Rng { seed: Rng |
   null }` needs no binder. Conflating "Rng is recursive" with "Inner is recursive"
   also corrupts the reserve/define machinery, since the alias-to-a-back-referencing-record
   drives a deferred union through an undefined id. So the rule is syntactic and local: the
   variable in the body, or nothing.

   *(An earlier draft had `opaque` module-scoping affect this — a record's visible field
   counting as a guard. Removed: it made `mu type Inner = Rng` well-formed, which it is
   not, and `opaque` no longer bears on contractivity at all.)*

   *One widening the rule as written does not describe:* a **newtype** counts as a guard
   during the contractivity walk, so the constructor can be reached through a name rather
   than written in the body. It is coherent — a newtype is a data constructor, and it is
   the one nominal sort the walk is not opaque to — but "written there" is not literally
   what the code tests.

2. Recursive references occur **only in covariant positions**. A function parameter is
   contravariant and therefore excluded; a return is covariant and allowed. A `throws`
   clause guards covariantly too, like a return.
3. **No recursion beneath negation or difference.**
4. The alias expands **equi-recursively — no runtime wrapper.** `mu type A = :ok | List[A]`
   and its one-step unfolding are the same type: no fold/unfold, no tag, no allocation.
   The backend carries the cycle as a layout back-edge, not as an operation.

Self-recursion only for v1; mutual recursion is a clear "not yet supported" error.

**A `mu type` with no recursive occurrence is an error.** The binder asserts recursion; if
there is none, either the binder is wrong or the type is.

*Against implicit recursion through plain `type`:* the restrictions above are unusual
enough to be worth a visible keyword, and a typo that creates accidental recursion should
be a plain error rather than a silently recursive type. A plain `type` alias that turns out
recursive is an error, and its message names `mu type`.

**`mu newtype` is banned, and a `newtype` may not be recursive.** `newtype T = List[T]` is
an error. Recursion is `mu type`'s job; a `newtype` is a nominal wrapper and nothing else.
A recursive *nominal* type is what `record` is for. Without an explicit ban, `newtype`
would acquire recursion by accident through the same lazy name-reference mechanism its
definitions table uses — a feature nobody designed, with none of the checks above.

*Where the code is thinner than this reads:* the recursion ban is a real diagnostic
(`RecursiveNewtype`). `mu newtype` is banned only by **absence** — the grammar has no such
production, so it is a generic parse error rather than a message saying the combination is
deliberately refused. The ban holds; the diagnostic does not explain itself.

**A nominal recursive record satisfies a structural μ-type — structurally.**
`record Node { next: Node | null }` satisfies `mu type T = { next: T | null }`. It costs
nothing extra, and it makes a structural μ-type a way to accept a whole *family* of nominal
recursive records without naming any of them.

### Atoms are singleton types

`:ok` is both a value and the type inhabited by exactly that value, so `:ok | :err | str`
is a union.

### `let x = :ok` has type `:ok`

    let x = :ok
    x = :err                     // error: :err is not a subtype of :ok
    let y: :ok | :err = :ok
    y = :err                     // fine

*Against widening to an `atom` supertype* (mirroring `let x = 1` widening to `i64`): that
would discard the singleton precision that makes `:ok | :err` unions and exhaustiveness
work, which is the entire reason atoms are types. An annotation is required exactly where
you intend to rebind, which is where one earns its keep. There is no `atom` primitive in
the type language, so there is nothing to widen *to*.

### Generic arguments are covariant

    List[i64]  <:  List[i64 | str]

Sound **because collections are values**. Covariance is only unsound for *mutable*
containers — that is why Java's arrays are broken and why Rust needs variance
annotations. An immutable `List[i64]` genuinely is a `List[i64 | str]`: there is no
operation that could write a `str` into it and be observed through the first type.

It is also not a rule the checker enforces separately: a generic argument is stored in a
reserved record field, and record subtyping is fieldwise, so covariance falls out of the
representation rather than being bolted on.

*Against invariance* (`List[i64]` and `List[i32]` unrelated, which a prior implementation
required): it contradicts μ-types. The rule that a recursive reference must occur in a
covariant position makes `mu type A = :ok | List[A]` illegal under invariance, because
`A` then sits in a non-covariant position — rejecting the canonical example. Invariance
also turns out to be a *lowering* constraint in disguise: `List_I64` and `List_Any` are
different C structs, and that leaked upward into the type system. Codegen's
representation problem is not the type system's.

*Against per-parameter annotations* (`record Box[+T]`): the only option if a mutable
container ever appears, and worth revisiting then. For now every collection is a value, so
there is nothing to annotate.

### A nominal record satisfies a structural parameter

`fn name_only(item: { name: str })` accepts `User { name: "Alice", age: 30 }`. This is what
makes structural parameters and record intersections (`{ a: i64 } & { b: str }`) useful
rather than decorative, and it is the natural reading of a set-theoretic type system.

### Sum types are unions of records; there is no `enum`

    record Red {}
    record Green {}
    type Color = Red | Green

Unit records parse, construct and match; empty records stay nominal, so `Red {}` and
`Green {}` are distinct despite sharing a shape. Exhaustiveness works through the union.
`enum` is not a keyword — it is an ordinary identifier, and the parser gives a dedicated
diagnostic when it appears as a declaration, because people will type it.

*Accepted costs:*
- **Variant namespacing.** Two sum types cannot each own a `Red`; record names are global,
  so a sum type puts N names at the top level. This is worse than it reads: a record's
  nominal identity is currently its **bare name**, with the declaring module dropped, so
  the collision is not merely stylistic — see `any` below and `TODO.md` item 1.
- **Binding patterns.** `Shape::Circle(r) => ...` has no equivalent; you re-destructure
  (`is Circle => { let c = s as Circle; ... }`). This is the real day-to-day cost —
  destructuring and record patterns are the compensating work, not optional extras.
- **Positional payloads** — tuple-ish variants have no encoding.
- **"A Color and nothing narrower" is inexpressible.** A bare `Red` is accepted where
  `Color` is expected (`Red <: Red | Green` — correct set-theoretically).

### Impls: own the protocol or the type; orphans only in the root application

    // a library may write:
    impl AnyProtocol for MyType      { }   // it owns the type
    impl MyProtocol for AnyType      { }   // it owns the protocol

    // it may not write:
    impl TheirProtocol for TheirType { }   // owns neither

    // the root application, and only the root application, may write:
    orphan impl TheirProtocol for TheirType { }

Coherence is only violated when two *dependencies* disagree about the same pair. There is
exactly one root application, so it cannot disagree with itself, and nothing can depend on
it and inherit the choice unknowingly. The escape hatch is therefore safe exactly where it
sits and unsafe anywhere else. `orphan` is explicit — the author says out loud that they
own neither side — and a library carrying one is rejected when used as a dependency.

**An orphan impl may only fill a gap.** Its target must be disjoint from every existing
impl of that protocol: `target ∧ ⋁ existing = ∅`. If something already covers those values,
the orphan is rejected — it cannot specialize, override, or steal them.

So orphans are strictly *additive*. They can make a protocol work for a type nobody
covered; they can never change what an existing impl does. Specialization by nesting is a
right reserved to whoever owns the protocol or the type — the only parties who can see the
whole picture. Without this, the root could quietly hijack a library's `impl Display for
Shape` for Circle values, and the library's own code would stop taking its own path.

**Targets may overlap only when nested**, one a subtype of the other, and the more specific
wins. `impl Area for Circle` and `impl Area for Shape` (`type Shape = Circle | Square`)
coexist; Circle values take Circle's impl. Partial overlap — `Circle | Square` and
`Square | Triangle`, meeting on Square — is rejected at declaration, because no value could
say which applies.

Specificity resolves **per value, not per static type**:

    let c: Circle = ...
    area(c)              // Circle's impl
    let s: Shape = c     // the same value, widened
    area(s)              // ...must still be Circle's impl

One value, one impl, regardless of what the checker happened to know. Nested-only overlap
is what makes "most specific" well defined: for any value the applicable impls form a
chain, so a unique minimum always exists. Where the receiver is not a subtype of a single
target, dispatch emits a runtime discrimination over the intersections rather than picking
statically.

*Against disjoint-only:* a library writing `impl Display for any` would lock every other
module out of `Display` permanently — the first wide impl wins and nobody else can ever
participate. Not a trade-off, a defect.

*Against no orphan rule:* adding a dependency could silently change which impl your values
take, and two libraries could impl the same pair with no principled winner.

**What is enforced today, and what is not.** The orphan rules are real: `orphan` in a
library is rejected, an orphan that is not disjoint from the existing impls is rejected,
and unnested overlap is rejected at declaration. The **ownership rule for plain impls is
not checked at all**, and cannot be yet — `use` does not load a dependency, so every
declaration the checker can see is local and the question has only one answer. Asking it
now would be theatre. It belongs here when dependency loading lands.

### `any` is ⊤, and there is no such thing as an erased type

`any` is the type inhabited by every value — the top type. It is **not** a marker for "the
checker could not work it out", and the type language cannot express that idea at all.

This is structural rather than a rule to remember: the checker's type representation has no
erased variant, so there is nothing to fall back *to*. Where a type cannot be determined,
the checker emits a diagnostic and poisons that expression; it does not return a type,
because no type means "unknown". Every remaining mention of `Erased` in the tree is prose
about the fallback that was removed.

Erasure is a **lowering** concern. A value of type ⊤ needs a uniform runtime
representation — that is a consequence of ⊤, not its meaning, and it is decided in codegen.

*Against conflating them* (`any` → an `Erased` type, as a prior implementation did): once
"the top type" and "I could not work it out" are the same value, every unknown silently
becomes `any`, and nothing distinguishes a deliberate `any` from a failure. In that
implementation roughly 70 of ~108 erased types were fallbacks rather than decisions, and
the consequences ran all the way to a stack-buffer-overflow on every `list::new()`.

**Protocols are bounds, never types.** `fn log(e: Error)` and `List[Error]` do not
typecheck: protocols live in their own table and a type position never consults it. The
rejection is real but the message is generic — "unknown type `Error`" — which reads as a
missing import rather than a category error. That line is deliberate; see "`main` does not
throw `any`" for what depends on it.

---

## Errors

### Checked `throws`, with the try / try? / try! triad

`try` propagates, `try?` softens to `T | null`, `try!` asserts. A single `catch` binds the
error union and matches inside it; there are no multi-catch clauses — the grammar admits
exactly one optional catch arm, so multi-catch is not merely unimplemented but unsayable.
A bare call to a throwing function is a compile error.

### `try` accepts a block

    try { a(); b() } catch (e) { ... }

Every throwing call inside is covered.

### `main` returns `()`, implicitly `throws Error`, and cannot say otherwise

    fn main() { ... }                           // the only form
    fn main() -> i64 { ... }                    // error: main's return type is fixed
    fn main() throws IoError { ... }            // error: main's throws clause is fixed

`main` returns `()`; the runtime wraps it to exit 0. Both halves of its signature are
fixed, and fixed means *written at all*: an explicit `-> ()` is an error, and so is
restating the implicit `throws Error`. There is one spelling of `main`.

It carries an implicit `throws Error`, never written and never changed — and since
every error record implements `Error`, any error propagates to `main`, so a bare
`try foo()` there always compiles without declaring anything. "Requires a compatible
enclosing `throws`" is therefore only a real constraint in *non*-`main` functions.

`main` is the catch-all and the runtime is its `catch`: an error reaching the top prints
the error's type and `Error::message()`.

Other exit codes come from `std::exit(n)`. A return value would be a second way to say what
`exit` already says, and would only work from `main`.

### Lookups `throws`; bracket indexing traps

    get(xs, i) -> T throws IndexError        // throws comes before ->
    try? map::get(m, k) orelse 30            // easy path
    try map::get(m, k) catch (e) { ... }     // when the distinction matters
    xs[i]                                    // traps on out-of-bounds. no try, no orelse.

*Against `get -> T | null`:* that is **unimplementable**, not just inconvenient. Unions
flatten and are idempotent, so for `List[i32 | null]`,

    get(xs, i) : (i32 | null) | null  ==  i32 | null

and "absent" is indistinguishable from "present, holds null". `throws` puts absence on a
separate channel, so nothing collapses. `try?` recovers the `orelse` ergonomics without
infecting the caller's signature, and the ambiguity reappears only where the programmer
opts into it by writing `try?` — which is a request to collapse the distinction.

`[]` traps rather than throws because a bounds violation is a bug, not a recoverable
condition, and a checked throw would force `try xs[i]` on every element access. A negative
index traps on the same check, not by wrapping — see "Indices are `i64`".

### Abnormal termination exits 101, on stderr

Traps, `try!` panics, and errors reaching `main` uncaught all exit **101**.

*Why not 1:* a program can exit 1 deliberately via `std::exit(1)`, and 1 is the
conventional generic failure code besides. If traps also exit 1, nothing can tell a program
that *chose* to fail from one that *died*.

---

## Syntax

### Rebindable `let`; no `mut`

Bindings rebind. Closures capture **by value, sealed** — a captured name cannot be rebound
through the closure. Shadowing is allowed.

### `and` binds tighter than `or`; `|>` binds tighter than comparison

    a or b and c         // a or (b and c)
    x |> f() == 3        // (x |> f()) == 3

A pipe is a call, and calls bind tighter than comparison. One precedence table, consumed by
both the parser and the formatter, so they cannot diverge.

### `else` is required when an `if` is consumed

Statement position without `else` is fine. In value position — let init, argument, return
position, block tail — a missing `else` is a compile error. There is no silent null
substitution.

### No default arguments — optional parameters are anonymous records

    fn connect(host: str, opts: { timeout: i64 | null, retries: i64 | null }) -> Conn {
        let t = opts.timeout orelse 30
        ...
    }

    connect(host, { timeout: 5 })

**A missing field satisfies a nullable field**, so `connect(h, {})` and
`connect(h, { timeout: 5 })` both typecheck, in any key order. Optionality rides on
`T | null` and `orelse`; there is no optional-field syntax and no `=` defaults.

*Against Elixir-style keyword lists with trailing `key: value` sugar:* records already do
the job with real types — `opts.timeout` is `i64` rather than requiring a list search and a
narrow of `(:timeout, i64) | (:retries, i64)` on every read. The trailing sugar is also a
silent-typo hole: `connect(h, timout: 5)` would build `{ timout: 5 }`, which satisfies the
parameter via width subtyping plus missing-nullable, so the typo compiles and takes the
default. Closing that needs exact field matching for one argument position, contradicting
width subtyping everywhere else. Writing the braces makes the record visible: if you wrote
them, the typo is yours to see.

*Accepted:* no ordering, no duplicate keys — `[where: a, where: b]` DSL tricks are not
expressible. And the one-optional-argument case is heavier than `punct: str = "!"` would be.

### One way to turn a value into a string

`Display` declares `to_string(v: T) -> str`, and `#{x}` desugars to `to_string(x)`. One
mechanism, two syntaxes. There are no `string::int_to_str`-style converters: a
monomorphic one can never cover a user's record, so keeping one means two mechanisms
forever.

*Why this needed deciding at all:* the corpus wrapped 289 of its 687 `println` calls in
`string::int_to_str`, and used interpolation in 7 files out of 201. That was not taste.
A prior implementation returned `Erased` from every protocol call except `eq` —
including every `to_string` — so `Display` did not work and interpolation could not. The
corpus routed around it and fossilised the workaround. `string::int_to_str` was a codegen
bug wearing a stdlib API's costume, which is the third time that shape has turned up
here after invariance and `T | null`. The converters are gone from the stdlib entirely.

`string::to_int` stays: parsing is not stringifying, and it throws `ParseError`. The pair
is asymmetric on purpose — `to_string` is total, `to_int` is partial.

### Generic inference is strict: no silent widening

A type variable binds to the first concrete type it meets and stays there. So
`push(xs, "s")` with `xs: List[i64]` pins `T := i64` from the list, and the `str` is a
mismatch — not a silent widening to `List[i64|str]`.

Widening a generic *is* sound for a covariant immutable collection: the result is a new
list and the original is untouched. It is also what Scala and Kotlin infer, and it is a
known papercut in both — a list quietly becomes `List[Any]` and nobody asked for it.
That silent surprise is exactly what the rest of this language is built to avoid, next
to no fallback to `any` and no erased type.

So widening is explicit. A turbofish (`push[i64|str](xs, "s")`) or the expected type
(`-> List[i64|str] { push(xs, "s") }`) sets `T` first, and the arguments then conform to
it. Inference is top-down before bottom-up for exactly this reason: the expected type is
applied before any argument is inspected, and a turbofish short-circuits inference outright.

The cost, stated plainly: it is mildly order-dependent — the first argument mentioning a
variable anchors it — and it rejects a few programs sound in theory, like `pair(1, "s")`,
which needs `pair[i64|str](1, "s")`. Every such rejection is a place the wide type was
probably unintended, and the escape hatch is one turbofish away.

*Where it still bites:* inference is first-wins and returns what it managed, without
checking that every variable was pinned. A call that leaves one unsolved reaches codegen as
an internal error rather than a diagnostic (`TODO.md` item 5). The decision is right; the
failure mode is not yet a message.

### Comparison is structural, and ordering is total within a type

*(Decided 2026-07-19, replacing "comparison operators are protocol calls" — see the note
at the end for what moved and why.)*

`==` and `!=` compare *structure*, always, on every type: primitives by value, `str` by
bytes, records fieldwise, tuples elementwise, lists elementwise and by length, maps by
content, unions by tag and then payload. No impl is required and none can override it.
`<`, `<=`, `>`, `>=` order the same way — lexicographically, records by field in
declaration order — so ordering is a property a type *has* by construction, and `sort(xs)`
needs no comparator.

There is no `Eq` protocol and no `Ord` protocol. A comparison is a primitive that the
backend expands per type, the same machinery that already builds map-key witnesses.

The reason is that equality and order are not choices. Two records with equal fields are
equal; there is no second defensible answer, and a protocol exists to let a type answer a
question its own way. `to_string` is a protocol because *formatting* genuinely varies —
that is a presentation choice. Equality does not vary, so the parallel that once justified
`Eq` does not hold. Pattern matching settles it: `case 1 =>` compiles to a structural
compare and could never dispatch to a user impl, so a dispatching `==` would have meant
two equality mechanisms that can disagree — the exact outcome the protocol rule was
written to prevent.

Ordering is the weaker half of the claim, and it is total by deliberate choice rather than
because a record has a natural order. Erlang and Elixir take this further and order
*across* types, so `1 < :atom` is true; that is the part to decline. Cross-type comparison
is almost always a mistake, and a total cross-type order makes it a silent one — Elixir
had to add compiler warnings for structural comparison of structs precisely because
`~D[2019-01-01] < ~D[2018-01-01]` returns a confident wrong answer. Answering where the
honest answer is "you did not say" is the thing this language does not do (see the `any`
rule, the required `else`, and record literals rejecting excess fields).

So the operands must overlap. `1 < "s"` and `P < Q` are diagnostics, as they already were.
The overlap test has one deliberate relaxation for equality: one operand being a *subtype*
of the other counts as comparable even when the meet is empty, because `xs == []` compares
`List[i64]` against `List[never]`, and `List[never]` has no inhabitants to intersect with —
the overlap test alone rejected the natural way to ask "is this empty".

Ordering a *union* is a diagnostic too: `(i64 | :none) < (i64 | :none)` typechecks under
the overlap rule but has no answer that is not an invented rank between the arms, and
inventing one would be the cross-type order sneaking back in through a side door. Union
*equality* is fine — compare tags, then payloads when they match.

**"Every type" is not literally every type**, and the exceptions are diagnostics rather
than wrong answers. Ordering recurses, so a type is ordered only when every part of it is:
an **atom** is a name rather than a magnitude, a **union** has no honest rank between its
arms, `null` has nothing to compare, and `Map`, a **closure** and a **self-referencing
record** are opaque pointers with nothing to walk. `List[T]` is ordered exactly when `T`
is, and a record when every field is. Equality is wider but not total either: a **closure**
is refused permanently, and so is a **union of two different records**, because the
field-reading path handles a single record atom and `A | B` normalises to two BDD paths of
which the second carries a negative. Relaxing that to walk every path was tried on
2026-07-19 and is not sufficient; it is tracked as lead L8 in `TODO.md`. Note that
`P | null`, `str | null`, `i64 | :none` and a union against a bare variant all compare
fine — they carry a tag and at most one record atom.

The checker and the backend answer this question from one module, and the backend panics
rather than emitting a comparison it cannot make, so a disagreement between them is a
compiler crash and not a bad answer.

*Consequences, stated because they bite:*

A type whose meaningful order differs from its structural order sorts wrong, and silently.
`{major, minor, patch}` happens to work; a date stored `{day, month, year}` does not, nor
does semver with prerelease tags, nor `Money { amount, currency }`. The escape hatch is
`sort_by(xs, key)` beside `sort(xs)` — one obvious function instead of a protocol, a
dispatch path, and seven natives.

`f64` makes "total" a slight lie at the leaf. NaN compares false against everything
including itself, so `NaN == NaN` is false, `{x: NaN} == {x: NaN}` is false — a record
that is not equal to itself — and **`sort` on a list containing NaN returns an unspecified
permutation**, not merely NaN in an odd position. IEEE-754 defines a `totalOrder` that
would fix sorting, and it was declined: it makes `0.0 == -0.0` false and `NaN == NaN` true,
trading a rare surprise for a common one. The operators do what the hardware does, which
is what every other language has taught people to expect. `sort` is where it shows.

*What moved:* the implementation never did dispatch — `check.rs` did an overlap test and
lowering emitted a primitive compare, while the prelude's `Eq`/`Ord` impls named seven
runtime symbols that do not exist and would not have linked. The doc lost. Four bugs fell
out of the gap and are fixed with this: `record == record`, `record < record` and
`tuple == tuple` each emitted C comparing two structs, which is not valid C; and
`list == list` compiled and returned pointer equality, so `[1,2,3] == [1,2,3]` was false.

Three further gaps this section once listed as open have since been closed by giving the
backend the comparison it was missing — `Map`, a self-referencing record, and a `List`
behind `null`. Two unions compared against each other once projected both sides to their
first variant, so `1 == true` was true at type `i64 | bool`; that is fixed and is now
tag-then-payload.

### Resources: cleanup is a value, not a keyword

*(Decided 2026-07-19. Implemented: `std::resource` and `std::fs::File` exist and the
`tests/lang/resources/` corpus passes. `docs/design/resources.md` is the spec.)*

    opaque record File { r: Resource[i64, IoError] }

A value that owns something outside the program -- a descriptor, a socket, a lock -- is held
in a `Resource[T, E]`: a refcounted runtime object carrying a payload, a cleanup function,
and an armed flag. It is an ordinary stdlib type in `std::resource`, not a language
construct.

Cleanup needs *identity*, and that is forced rather than chosen. Neon is ARC with value
semantics, so an inline record is copied freely and "the last reference dies" is undefined
for one. The refcount pass does hook a record's last use -- that is how a `str` field gets
released -- but it fires once per copy, which for a user-supplied cleanup means a double
close. Rust escapes this with affine types (`Copy` and `Drop` are mutually exclusive);
Swift, which is ARC with copyable values exactly as Neon is, allows `deinit` only on
classes. Neon takes Swift's answer, because linearity would split the type system into
copyable and non-copyable and every generic, container and closure would then have to say
which it accepts.

Both paths work and neither is the fallback:

    try fs::close(file)     // explicit: disarms, closes, and you see the error
    // or just stop using it: cleanup runs at its last use

`E` is the error cleanup may throw, carried on the arrow type. That choice pays three
times: `release` composes with `try`; infallible cleanup is `Resource[T, never]` whose
`release` needs no `try` at all; and double-release needs no sentinel, because `release`
disarms first and a second call has nothing left to run.

*Consequences, stated because they bite:*

**Only the explicit path can observe failure.** Drop has no error channel, so automatic
cleanup discards the error. That is the answer to "why have `close` when it closes itself" --
a question Rust never answered well, since its `File` has no `close` at all.

**Cleanup fires at the last *use*, not at scope exit.** Earlier than RAII, which is good for
a descriptor and fatal for a lock guard: `let g = lock(m)` whose handle is never touched
again releases immediately, before the section it was meant to protect. Scope-lifetime
resources are not expressible under last-use ARC.

**A resource in a cycle is not prompt.** It closes when the collector runs. Every other path
is deterministic; this one is the exception, and it is a property of having a collector at
all.

**A trap skips cleanup entirely** -- `neon_trap` calls `_exit` with no unwinding, by design,
so buffered writes are lost on the way out.

**The guard rests on `opaque`, which does not yet hold.** `opaque record File` is what stops
a caller reaching `f.r` and releasing the descriptor behind the module's back. Field access
*is* checked — but a record's nominal identity is its bare name with the declaring module
dropped, so a second module declaring `record File` declares the *same type* and can forge
one. That is `TODO.md` item 1, and until it is fixed the encapsulation this design assumes
is a convention rather than a guarantee.

### The compiler learns a type's representation from an annotation, not a name

`record_repr` matched the literal strings `"List"` and `"Map"` to give them runtime
representations, and adding `File` to that table was what made file handles magic.
`@runtime("neon_list")` on the declaration replaces it: the marker travels with the type,
so a new runtime-backed type is a stdlib declaration rather than a compiler edit, and
`File` becomes an ordinary record holding a `Resource`.

*Not finished.* The special-case count went from three to **two**, not to zero. `List` and
`Map` are still matched by literal name in `record_repr`, because their element types drive
witness emission and the codegen-assisted natives, so they move separately; they are also
matched by name in `typecheck/ordered.rs`. Moving them out is `TODO.md` item 17, and the
bare-name matching is leads L1/L2 there.

Stdlib-only, like markers. It names a C type the backend must already know, and pointing it
at one that does not match the expected ABI is the same hazard `@native` carries.

### Markers: a bound with no methods

*(Decided 2026-07-19, alongside structural comparison.)*

    marker Ord

    fn max[T](a: T, b: T) -> T where T: Ord { if a < b { b } else { a } }

A **marker** is a bound carrying no methods, satisfied by a rule the compiler knows rather
than by an impl. `where T: Ord` says "this parameter can be ordered"; the compiler answers
it from `T`'s structure. There is nothing to write and no way to override it.

This exists because a generic body is checked *once*, with `T` abstract. There is no
concrete type there to test, so `a < b` on a bare `T` has no answer — and the old compiler,
which allowed it, monomorphised `max(map, map)` into a comparison of two addresses. The
bound supplies the missing information at the boundary where it exists: the body is checked
against the bound, and each call site is checked against the type actually supplied.

A marker is not a protocol wearing a hat. A protocol is a *choice* a type makes about how to
answer something — that is why `Display` is one. A marker is a *fact* about a type that the
type does not get a say in: `Ord` follows from structure, and letting a type claim it while
holding a `Map` would be a lie the backend cannot honour. Markers are prelude-only — only
the compiler can supply a rule, so a marker it does not recognise is a diagnostic at the
declaration, and `Ord` is the only rule that exists.

*One claim this section used to make that the code does not:* `impl Ord for X` is described
here as *unwritable*. It is not. Nothing in the impl path checks whether the protocol is a
marker, and a marker has no methods to leave unimplemented, so `impl Ord for X {}` is
accepted silently and has no effect — satisfaction short-circuits to the compiler rule
regardless. Only the bound-failure message claims otherwise ("cannot be made to"). The
decision stands and the impl is inert, but "unwritable" describes the intent, not the
compiler. Relatedly, the marker rule is keyed on the bare name `"Ord"`, so a user
`marker Ord` in another module may inherit the built-in rule — lead L1 in `TODO.md`.

**Order is infectious, and the bound threads through the recursion.** A record is ordered
when every field is, `List[T]` when `T` is. That means a *bound* variable must count as
ordered inside a container too, or the marker would be useless past the first one:
`Box[T]` under `where T: Ord` is ordered precisely because `T` is bound there. Ask the same
question without that context and the answer is no.

The ceremony is real and was weighed: nearly every type is ordered, so the bound excludes
few types while appearing on many signatures. It buys a general mechanism rather than a tax
on one operator — `Send` and friends land here later — and the alternative, inferring the
requirement, makes a function body silently change its public contract and needs a fixpoint
over the call graph to propagate. For a type whose meaningful order is not its structural
one, nothing here is a dead end: `max_by`/`sort_by` take the comparison as an argument and
need no bound at all, which is also why `Ordering` stayed in the prelude.

Unlike ordering, **equality takes no bound**: it is total by design, so there is no `Eq`
marker, and a generic `a == b` on an abstract `T` is allowed and deferred. Requiring a
marker there would contradict the decision.

### Names count what they say

`string::byte_len`, not `string::len`. It counts bytes — `byte_len("é")` is 2 — and a
name is where that surprise belongs. A comment on the declaration is not read at the
call site. `list::len` and `map::len` keep `len`: elements are the only unit they could
mean.

### There is a prelude, and it holds only what syntax needs

Interpolation is syntax and desugars to a protocol call, so without a prelude every file
containing a string hole needs an import before a language feature works. The rule: **if
you can write it without naming it, it is in the prelude.** `io::println` still needs
`use std::io` — it is a function, not syntax.

`Eq` and `Ord` were *protocols* here while `==` and `<` were meant to dispatch. Comparison
is structural now, so `Eq` is gone entirely and `Ord` survives only as a marker — a bound a
generic writes, with no methods and nothing to implement.

What is actually there, and the reason each one clears the bar:

- **`Display`, `Error`** — the two protocols. `#{x}` desugars to `to_string(x)`; `main`'s
  implicit channel demands that whatever escapes implements `Error`.
- **`Ord`** — the marker the `where T: Ord` bound names.
- **`List`, `Map`** — declared here because `record_repr` matches these two names literally
  and they must be unambiguous. This one is *not* "syntax needs it", and it is the reason
  the root has to be excluded from opacity nesting; moving them out is `TODO.md` item 17.
- **`range`** — `for i in range(a, b)` is the counted-loop idiom, and an import in front of
  every loop is worse than a prelude name.
- **`IndexError`, `Ordering`** — each is shared by two stdlib modules with no single owner
  to move it to. A `std::error` module would be `IndexError`'s home if one existed.

So the earlier line here — "`Display` and `Error`, plus the `Ord` marker. Nothing else." —
was never true of the code. Three of the seven names are there for reasons other than
syntax, and two of those are load-bearing accidents rather than decisions.

### String interpolation is `#{expr}`

    "count: #{n}, json: { \"literal\": true }"
    "#{Point { x: 1, y: 2 }}"        // brace-matched; record literals are fine
    "#{fmt::pad(x, 8)}"              // formatting is functions

*Against `{expr}`:* `{` already means records and blocks, so `{}` in a string collides with
both the record literal and a `{{`/`}}` escape — `"{Point { x: 1 }}"` is genuinely
ambiguous. With `#{`, a bare `{` is always literal and needs no escaping; only `#{` is
special, and `\#{` escapes it.

**A hole holds any expression**, terminated by brace matching. Nested quotes are fine
(`#{f("b")}`).

**There are no format specs.** A colon cannot mark one: `#{:ok}` is an atom, so the colon
appears in leading position, and `#{m[:key]}` puts one mid-expression. Reserving `:` would
mean deciding where the expression ends before the lexer knows.

*Broken today:* interpolating a call that resolves through a protocol miscompiles — the
hole's `to_string` resolution is written to the hole expression's own id and overwrites the
call's. `TODO.md` item 2 has the repro.

### Annotations are `@name` or `@name("string")`

    @native("neon_str_len") fn len(s: str) -> i64
    @cfg("not(windows)") fn spawn(...)
    @doc("Adds two numbers.") fn add(a: i64, b: i64) -> i64

One shape for all of them. **`@cfg` takes a string**, not a nested expression:
`@cfg("not(windows)")`, not `@cfg(not(windows))`. Whatever evaluates the cfg parses its
contents; the grammar needs no expression language of its own for a corner nobody reads.

### Comments and blank lines survive lexing

The lexer emits tokens *and* a side table of trivia: every comment with its span and text,
plus the offset of each line start. The parser's input is unchanged — no combinator steps
over trivia.

Without this a formatter can only delete comments, which is the failure the previous
implementation could not get out from under. Blank lines the author left between items are
recovered from line starts rather than by recording whitespace; a formatter that reflows
them all on first run is not one people will use.

`///` is tagged as a doc comment at lex time. Distinguishing it later means re-lexing.

*Against a lossless CST* (every token and space in a concrete tree, the AST a view over
it): perfect fidelity and the right answer for an LSP, but a different parser architecture.
A side table is lossless in *data* — attachment (leading vs trailing) is computed from
spans by the consumer, where it can be revised, rather than frozen into the parser.

### Tests are `test "name" { ... }` blocks with assert intrinsics

    test "adds two" {
        assert_eq(add(1, 1), 2)
    }

    bench "push 1k" { ... }

`assert`, `assert_eq`, `assert_ne`, `assert_throws` are **intrinsics**, not stdlib
functions: the compiler knows them, so a failure can report the actual values and the
source span. An ordinary function cannot see its argument's source text. `test` and `bench`
blocks are stripped from normal builds.

The reporting is what the choice was for, and it is what a failure prints:

    test arithmetic is broken ... FAILED
        assertion failed: 1 + 1 == 3
          left:  2
          right: 3

`assert(a == b)` is not lowered as one opaque condition. When the argument is a comparison,
lowering splits it, evaluates both sides, and reports both — so `assert(1 + 1 == 3)` says as
much as `assert_eq(1 + 1, 3)`. The expression text is reconstructed from the AST (a small
renderer in `ir/lower.rs`, bracketing off the shared precedence table, not the formatter,
which needs a token stream and a comment table it does not have here). Values are rendered
through the same `to_string` symbols string interpolation uses; a repr with no `to_string`
prints `<not displayable>` rather than a fake.

### One process per test

`neon test` compiles the file once, with `test` blocks lowered as nullary functions and a
generated entry point that dispatches on `NEON_TEST`, then spawns that binary once per
block.

*Against a generated `main` that walks a table of every test:* a failed assertion calls
`neon_panic`, which exits the process, and the language has no way to recover from a panic.
An in-process harness would report the first failure and then be gone. One process per test
is what makes "report both, name the failing one" possible at all, and it contains a
segfault or a corrupted heap exactly as well as it contains an assertion.

It also settles `main`: the entry point of a test build is generated, so a file holding only
tests compiles and runs. A `main` that *is* present is compiled and never called.

The selector is an environment variable rather than `argv` because every Neon program's
entry point is `int main(void)`; `getenv` reaches the same information without giving test
binaries a different entry signature from real ones.

*Not working:* `assert_throws`. Its argument is a throwing call, which the checker rejects
outside a `try` — there is no well-typed program that reaches its lowering, so it is still a
`<todo:>` marker. Making it work needs the checker to treat `assert_throws` as a throw sink.
`bench` blocks are parsed and stripped, and nothing runs them.

---

## Values

### `str` is UTF-8 bytes

Indexing and slicing are byte-oriented. Validation happens at IO boundaries.

### Source and identifiers are UTF-8

    let café = 1
    let 日本語 = 2

Identifiers follow **UAX #31** — `XID_Start` / `XID_Continue` — and are normalized to
**NFC**. Atoms follow the same rules. A character that is neither `XID_Start` nor
punctuation is an `unexpected character`, not a mangled identifier.

Normalization is not cosmetic: without it `café` (U+00E9) and `café` (`e` + U+0301) are
different identifiers that render identically, which is a way to smuggle a second
definition past a reader.

*Open:* confusable and mixed-script detection. NFC does not catch Cyrillic `а` posing as
Latin `a`, and nothing in the compiler looks for it.

### Indices are `i64`, and there is no negative indexing

    fn get[T](list: List[T], i: i64) throws IndexError -> T
    list::last(xs)              // not xs[-1]

*Against `usize`:* the common index bug is a computed `i - 1` at `i = 0`, not a literal
`xs[-1]`. With `i64` that traps as `index out of bounds: -1` and you see it instantly; with
`usize` and wrap-on-overflow it traps as `index out of bounds: 18446744073709551615` and
you have to decode it. Making `usize` good would need trapping subtraction — an exception
to the integer semantics below. `usize` is also `size_t` leaking through the API: lists are
values here, indices are just numbers, and `i64` loses no range.

*Against Python-style negative indexing:* it converts that same `i - 1` bug from a trap into
a **silent wrong answer** — reading the last element instead of failing. It also puts an
`if i < 0 { i += len }` branch on every element read, in the hottest loop.

### Integers: C11 truncation

The runtime targets **C11**.

- `+ - *` and unary `-` **wrap** on overflow, implemented as an unsigned round-trip so the
  wrap is defined rather than inherited from signed-overflow UB.
- `/` truncates toward zero; `%` takes the dividend's sign: `-7 / 2 == -3`, `-7 % 2 == -1`.
- `/` and `%` **trap** on divisor 0 and on `INT64_MIN / -1`, never raising SIGFPE.
  `INT64_MIN % -1` traps too, even though the answer is mathematically 0 — one rule, one
  check, and `/` and `%` never disagree about when they trap.
- `bsl`/`bsr` **mask** the shift amount to the operand's width: `1 bsl 200 == 1 bsl 8 == 256`.

**A folded expression and the same expression computed at runtime always agree** — but by
*declining*, not by mirroring. The folder is strictly more conservative than the runtime:
`Add`/`Sub`/`Mul` use checked arithmetic and simply do not fold on overflow, where the
runtime would wrap (a missed fold, nothing observable); `Div`/`Rem` decline on both
trapping inputs, which is load-bearing, since folding would replace a trap with a value or
abort the *compiler* over code that never runs; and shifts are not folded at all. Only
`Neg` folds with the same wrapping the runtime uses. The guarantee holds; "follows exactly
these rules" describes the intent rather than the mechanism.

*`bsr` was specified as type-driven* — arithmetic on signed, logical on unsigned, because
the type already knows which you meant, with codegen obliged to *guarantee* it rather than
inherit it, since C11 §6.5.7p5 makes `>>` on a negative signed value implementation-defined.
Neither half is met. The language has no unsigned integer type at all, so "logical on
unsigned" is vacuous; and codegen emits a bare C `>>` on `int64_t`, which is precisely the
implementation-defined behaviour the rule exists to avoid. It works on gcc and clang by
their choice, not by the compiler's. This is an undischarged intention, not a shipped rule.

*Against floor division (Python):* nicer for modular arithmetic, but costs an emitted
correction on every division.

### `orelse` tests a nullable union's tag

Never "if truthy". If the left type cannot contain `null` — a bare `i64` — then
`A orelse B` is always `A`, **including when A is `0`**. Lowering branches on a tag test,
not on a comparison against zero, which is the bug this replaced.

### Combinator pipelines interleave effects, element by element

`map`, `filter`, `fold` and their kin (the `Mappable` family) run **one element
through the whole pipeline before the next**, left to right. `filter(map(xs, f), p)`
evaluates `f(x₀), p(f(x₀)), f(x₁), p(f(x₁))…`, not every `f` and then every `p`. This
is the iterator ordering Rust defines, and it is the *definition* here, not an
observable consequence of some implementation.

The point is what it licenses. A pipeline is otherwise N traversals of one buffer —
`rc == 1` reuse means no per-stage allocation, so the cost is passes, not heap — and
the compiler is free to **fuse a statically visible chain into a single loop**. Fusing
reorders cross-stage effects, and normally that would need proving the stages pure to
license it. Making the interleaved order the *specification* removes the question:
there is no other order to preserve, so nothing has to be proven and no purity is
tracked. (A dynamic `Mappable` value the compiler cannot see through falls back to
eager per-stage; same result, more passes.)

This is reserved now and unused now. v1 lowers eagerly, per stage — Elixir's `Enum`,
which is fine because the passes are cheap. `impl Mappable for List` allocates a fresh
list and pushes in a `for` loop, and there is no fusion pass. The stdlib signatures do not
depend on the choice, so fusion can arrive later against an IR with zero change to any
program. What could *not* arrive later is the permission: code that relied on "every `f`,
then every `p`" would break under fusion, so the ordering is fixed here before anyone can
write that code.

If you need one stage's effects fully sequenced before the next, that is a `for` loop,
not a pipeline. Pipelines are for transforming values, not for ordering effects.

### A top-level `const` is compile-time or it is an error

    const LIMIT: i64 = 42
    const DOUBLE: i64 = LIMIT * 2

A `const` has no storage, no address and no initialisation order, because it produces no
runtime object. Lowering re-lowers the initialiser at each use and the folder in `ir::opt`
collapses it there; a `str` const becomes `neon_str_lit`, so its bytes land in `.rodata`
like any literal written in place.

**The type is required.** Inferring it would mean typing initialisers in dependency order
before any of their types were known — `const B = A + 1` cannot be typed until `A` is —
which is a lot of machinery to save an annotation on module-level API.

**The initialiser admits exactly what the folder folds:** literals, other consts, and
integer and boolean arithmetic over those. Float and string arithmetic are rejected, and
this is the part that surprises people — `"a" + "b"` is as constant as `1 + 2` to a reader,
and is refused because `fold_int` and `fold_bool` are the whole of the folder.

That rule exists because the failure is otherwise *silent*. An initialiser the folder
declines still compiles and still runs; it just becomes runtime work performed at every
use, which is the opposite of what `const` promises. A diagnostic that names the obstacle
is worth more than a constant that quietly is not one. The same reasoning is why adding
`const` was paired with teaching the folder `bsl`/`bsr` and `bnot` rather than rejecting
them: `const MASK: i64 = 1 bsl 20` should be a constant, so the folder learned to make it
one — and `verify/src/fold.rs` proves the new arms match the emitted C for every operand.

A `const` that refers to itself, directly or through others, is rejected before lowering.
It has to be: lowering inlines initialisers, so a cycle would not terminate.

---

## Implementation

### The lexer is hand-written

*Against a lexer generator:* it earns roughly 30% of the work — keywords, the ident and
atom regexes, the outer shape of numbers and strings — and constrains the rest. Integer
parsing, string escapes, rune escapes and nested block comments all end up as hand-written
callbacks anyway, because a regex cannot count.

It also cannot do interpolation: `#{}` needs a mode stack with brace-depth counting and
string-state tracking, and a DFA matcher is stateless. The hybrid — a generator wrapped in a
hand-written mode driver — is more complex than either pure option.

Two costs a generator imposes concretely: the error channel is one type per token, so
literal parsers return `Option` and a bad escape becomes "unrecognized" with no position;
and an `Int(i64)` token makes `-9223372036854775808` unlexable, because the magnitude
overflows before the parser can fold the sign. The token carries a `u64` magnitude instead.

Speed is irrelevant — lexing is never a compiler's bottleneck.

*(Whether block comments should exist at all is still open — they nest, which is correct,
and nesting is the only reason the tree-sitter external scanner exists. `TODO.md` item 16.)*

### The compiler library never touches the filesystem

Source arrives as an in-memory map of module path to text. The CLI and LSP read files; the
library receives them already read.

### The runtime and stdlib are data the toolchain ships

Nothing is baked in at compile time. The sysroot is resolved at runtime relative to the
executable, so dev and installed builds share one code path, and a relocated binary fails
with a clear error rather than pointing at a build directory that does not exist.

**The stdlib source lives on disk, not embedded.** `stdlib/std/io.neon` is the module
`std::io`, by path — no `mod std { mod io { } }` wrapper. It is loaded as real files with
real paths, because an LSP resolving go-to-definition into `println` must be able to open
an actual file and show a span in it. Embedding the source in the binary would make every
stdlib location synthetic, and the stdlib will hold real Neon code, not only `@native`
signatures, before long. Tests reach it the way `corpus_roundtrip` reaches the corpus —
a `CARGO_MANIFEST_DIR`-relative path — so no sysroot is needed under `cargo test`.

Module path is derived from file path: this is the one place a file-to-module mapping
exists, and it costs nothing, because `Env::declare` already takes a module prefix (it is
how nested `mod` blocks work). The loader walks `stdlib/`, turns `std/io.neon` into the
prefix `std::io`, and declares each file's decls under it before the user's module.

*One thing this does not yet buy:* a diagnostic in a stdlib file renders against the
*user's* file at a fabricated location, because errors from every module are sorted into
one renderer holding one file. `TypeError` needs a file id. `TODO.md` item 13.

### `Error` owns `message`; `Display` is a separate concern

`protocol Error for T where T: Display {}` welded two unrelated jobs together. `Display`
answers "how does this value render" — a `User` renders as `"Alice"`. An error answers a
different question, "what went wrong", and wants `"failed to load user Alice: connection
refused"`. One `to_string` cannot serve both, and a type that is both a value and an error
had to pick. The protocol also declared no methods of its own, so it was, literally,
"`Display`, but you also had to write an `impl`" — the marker and the capability doing
unrelated work under one name.

    protocol Error for T {
        fn message(v: T) -> str
    }

`message` is the one thing an error genuinely knows about itself. Everything else people
reach for -- context, a stacktrace -- is a property of *where it was thrown*, which the
value cannot know. Those belong to the throw, not the type, and a default method body
cannot supply them: a default can compute from fields the type already has, but it cannot
create storage.

*(Default method bodies are, separately, never typechecked at all — the protocol's subject
is unbound when they are checked. `TODO.md` item 6.)*

### `throws` is unconstrained; `Error` is required only at the top

`throws T` accepts any type: `throws any`, `throws :ok`, `throws Person`, `throws bool`.
A `throws` clause is a claim about what a function can fail with, not a claim that the
failure is presentable. `throws E ... where E: Error` covers generic propagation and
already works, because the clause resolves in the function's rigid scope.

The `Error` bound applies at exactly one place: an error escaping `main`. That is the only
point where the language must render something it did not author, so it is the only point
that may demand an interface. A non-`Error` reaching it is a compile error telling you to
catch it or implement `Error`.

*(The unconstrained half is true by construction — the `throws` clause is resolved with no
bound check anywhere — but nothing in the corpus writes `throws :ok` or `throws bool`, so
it is unexercised.)*

### `main` does not throw `any`

`main`'s implicit `throws Error` needed a type, and `Error` is a protocol -- a bound, not a
type -- so ⊤ was substituted. That cost two things. It boxed the error path; and because
everything is a subtype of ⊤, it silently switched off the check that a thrown value is an
error at all, so `throw 42` from `main` compiled.

`main`'s error channel is therefore **not a type**. It is a rule, checked per throw site.
No existential is needed to enforce it: at every escape point the concrete type is
statically known (or is a concrete union), so `message` is a direct call and a union is a
tag switch. There is no vtable, no box, and nothing named `Report`. `throw 42` from `main`
is now rejected, because resolving `message` for `i64` fails.

This holds precisely as long as **protocols stay bounds and never become types**. The day
`List[Error]` or `fn log(e: Error)` is expressible, a value of unknown type must be carried
at run time and a real protocol object is required. That line is deliberate.

### `any` may only come from source

`any` is legitimate when written: `throws any` asks for erasure and gets a box. The failure
mode is the compiler *inventing* it where nobody wrote it -- a fallback, a default, an
unhandled case -- and then using ⊤'s properties to answer questions it was never asked. That
is how the previous compiler died, and it recurred here twice: `lower_try` hardcoded
`Repr::Any` for the handler parameter, and `wrap_throwing` was handed the callee's declared
error type and discarded it.

`Repr::Any` must only ever arise from a source type that genuinely is ⊤, never from a
fallback -- guarded the way `Repr::Var` already is.

*Not yet true.* On the layout path it holds: `Repr::Any` is produced only under a genuine
top test, and the places that would otherwise guess — a list literal at an unresolved repr,
an `is` on an erased value with no resolved tag — ICE rather than invent. But
`repr_from_typespec`, which serves the turbofish path, has a bare `_ => Repr::Any` arm
covering every tuple and arrow typespec, and a closure parameter with no repr falls back the
same way. That function feeds monomorphisation *identity*, so the fallback collapses
distinct instances into one symbol; it is documented in place as a known defect and tracked
as `TODO.md` item 12, alongside the wider class of lossy projections used as identities.
Until those arms are gone, this section states the rule, not the state.
