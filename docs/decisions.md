# Decisions

The corpus (`tests/lang/`) says **what** the language does. This says **why**, and what was
rejected.

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

2. Recursive references occur **only in covariant positions**. A function parameter is
   contravariant and therefore excluded; a return is covariant and allowed.
3. **No recursion beneath negation or difference.**
4. The alias expands **equi-recursively — no runtime wrapper.** `mu type A = :ok | List[A]`
   and its one-step unfolding are the same type: no fold/unfold, no tag, no allocation.

Self-recursion only for v1; mutual recursion is a clear "not yet supported" error.

**A `mu type` with no recursive occurrence is an error.** The binder asserts recursion; if
there is none, either the binder is wrong or the type is.

*Against implicit recursion through plain `type`:* the restrictions above are unusual
enough to be worth a visible keyword, and a typo that creates accidental recursion should
be a plain error rather than a silently recursive type. A plain `type` alias that turns out
recursive is an error.

**`mu newtype` is banned, and a `newtype` may not be recursive.** `newtype T = List[T]` is
an error. Recursion is `mu type`'s job; a `newtype` is a nominal wrapper and nothing else.
A recursive *nominal* type is what `record` is for. Without an explicit ban, `newtype`
would acquire recursion by accident through the same lazy name-reference mechanism its
definitions table uses — a feature nobody designed, with none of the checks above.

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
you intend to rebind, which is where one earns its keep.

### Generic arguments are covariant

    List[i64]  <:  List[i64 | str]

Sound **because collections are values**. Covariance is only unsound for *mutable*
containers — that is why Java's arrays are broken and why Rust needs variance
annotations. An immutable `List[i64]` genuinely is a `List[i64 | str]`: there is no
operation that could write a `str` into it and be observed through the first type.

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
  so a sum type puts N names at the top level.
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
chain, so a unique minimum always exists.

*Against disjoint-only:* a library writing `impl Display for any` would lock every other
module out of `Display` permanently — the first wide impl wins and nobody else can ever
participate. Not a trade-off, a defect.

*Against no orphan rule:* adding a dependency could silently change which impl your values
take, and two libraries could impl the same pair with no principled winner.

### `any` is ⊤, and there is no such thing as an erased type

`any` is the type inhabited by every value — the top type. It is **not** a marker for "the
checker could not work it out", and the type language cannot express that idea at all.

This is structural rather than a rule to remember: the checker's type representation has no
erased variant, so there is nothing to fall back *to*. Where a type cannot be determined,
the checker emits a diagnostic; it does not return a type, because no type means "unknown".

Erasure is a **lowering** concern. A value of type ⊤ needs a uniform runtime
representation — that is a consequence of ⊤, not its meaning, and it is decided in codegen.

*Against conflating them* (`any` → an `Erased` type, as a prior implementation did): once
"the top type" and "I could not work it out" are the same value, every unknown silently
becomes `any`, and nothing distinguishes a deliberate `any` from a failure. In that
implementation roughly 70 of ~108 erased types were fallbacks rather than decisions, and
the consequences ran all the way to a stack-buffer-overflow on every `list::new()`.

---

## Errors

### Checked `throws`, with the try / try? / try! triad

`try` propagates, `try?` softens to `T | null`, `try!` asserts. A single `catch` binds the
error union and matches inside it; there are no multi-catch clauses. A bare call to a
throwing function is a compile error.

### `try` accepts a block

    try { a(); b() } catch (e) { ... }

Every throwing call inside is covered.

### `main` returns `()`, implicitly `throws Error`, and cannot say otherwise

    fn main() { ... }                           // the only form
    fn main() -> i64 { ... }                    // error: main's return type is fixed
    fn main() throws IoError { ... }            // error: main's throws clause is fixed

`main` returns `()`; the runtime wraps it to exit 0. Both halves of its signature are
fixed. It carries an implicit `throws Error`, never written and never changed — and since
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
condition, and a checked throw would force `try xs[i]` on every element access.

### Abnormal termination exits 101, on stderr

Traps, `try!` panics, and errors reaching `main` uncaught all exit **101**.

*Why not 1:* a program can exit 1 deliberately via `std::exit(1)`, and 1 is the
conventional generic failure code besides. If traps also exit 1, nothing can tell a program
that *chose* to fail from one that *died*.

---

## Syntax

### Rebindable `let`; no `mut`

Bindings rebind. Closures capture **by value, sealed**. Shadowing is allowed.

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
here after invariance and `T | null`.

`string::to_int` stays: parsing is not stringifying, and it throws. The pair is
asymmetric on purpose — `to_string` is total, `to_int` is partial.

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
it. Inference is top-down before bottom-up for exactly this reason.

The cost, stated plainly: it is mildly order-dependent — the first argument mentioning a
variable anchors it — and it rejects a few programs sound in theory, like `pair(1, "s")`,
which needs `pair[i64|str](1, "s")`. Every such rejection is a place the wide type was
probably unintended, and the escape hatch is one turbofish away.

### Comparison is structural, and ordering is total within a type

*(Decided 2026-07-19, replacing "comparison operators are protocol calls" — see the note
at the end for what moved and why.)*

`==` and `!=` compare *structure*, always, on every type: primitives by value, `str` by
bytes, records fieldwise, tuples elementwise, lists elementwise and by length, unions by
tag and then payload. No impl is required and none can override it. `<`, `<=`, `>`, `>=`
order the same way — lexicographically, records by field in declaration order — so every
type is ordered and `sort(xs)` works on any list without a comparator.

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
Ordering a *union* is a diagnostic too: `(i64 | :none) < (i64 | :none)` typechecks under
the overlap rule but has no answer that is not an invented rank between the arms, and
inventing one would be the cross-type order sneaking back in through a side door. Union
*equality* is fine and stays total — compare tags, then payloads when they match.

*Consequences, stated because they bite:*

A type whose meaningful order differs from its structural order sorts wrong, and silently.
`{major, minor, patch}` happens to work; a date stored `{day, month, year}` does not, nor
does semver with prerelease tags, nor `Money { amount, currency }`. The escape hatch is
`sort_by(xs, key)` beside `sort(xs)` — one obvious function instead of a protocol, a
dispatch path, and seven natives.

Ordering recurses, so a type is ordered only when every part of it is. `Map` has no order
(opaque, pointer-backed), `List[T]` is ordered exactly when `T` is, and a record that
reaches itself is a pointer with nothing to walk. All three read as one ordered shape at
the top level, and all three are diagnostics.

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
holding a `Map` would be a lie the backend cannot honour. So `impl Ord for X` is not merely
unnecessary, it is unwritable, and markers are prelude-only — only the compiler can supply
a rule, so a marker it does not recognise is a diagnostic at the declaration.

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

*Where the code has not caught up yet, stated so this section does not drift ahead of it a
second time:* `==` is structural today on primitives, `str`, records, tuples, lists and
unions. It is **not** yet on `Map`, on a nullable pointer, or on a closure, and a union
compared against another union projects both to their first variant instead of switching on
the tag. Those four predate this decision and are unchanged by it — none is a new
regression — but until they are fixed "total on every type" describes the decision, not the
compiler. `docs/finalpush.md` has the table and the mechanism for each.

### Names count what they say

`string::byte_len`, not `string::len`. It counts bytes — `byte_len("é")` is 2 — and a
name is where that surprise belongs. A comment on the declaration is not read at the
call site. `list::len` and `map::len` keep `len`: elements are the only unit they could
mean.

### There is a prelude, and it holds only what syntax needs

`Display` and `Error`, plus the `Ord` marker. Nothing else.

`Eq` and `Ord` were *protocols* here while `==` and `<` were meant to dispatch. Comparison
is structural now, so `Eq` is gone entirely and `Ord` survives only as a marker — a bound a
generic writes, with no methods and nothing to implement.

Interpolation is syntax and desugars to a protocol call, so without a prelude every file
containing a string hole needs an import before a language feature works. The rule: **if
you can write it without naming it, it is in the prelude.** `io::println` still needs
`use std::io` — it is a function, not syntax.

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
Latin `a`.

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

- `+ - *` and unary `-` **wrap** on overflow.
- `/` truncates toward zero; `%` takes the dividend's sign: `-7 / 2 == -3`, `-7 % 2 == -1`.
- `/` and `%` **trap** on divisor 0 and on `INT64_MIN / -1`, never raising SIGFPE.
  `INT64_MIN % -1` traps too, even though the answer is mathematically 0 — one rule, one
  check, and `/` and `%` never disagree about when they trap.
- `bsl`/`bsr` **mask** the shift amount to the operand's width: `1 bsl 200 == 1 bsl 8 == 256`.
- `bsr` is **type-driven**: arithmetic on signed, logical on unsigned. The type already
  knows which you meant. Codegen must guarantee this rather than inherit it — C11 §6.5.7p5
  makes `>>` on a negative signed value implementation-defined.

Constant folding follows exactly these rules, so a folded expression and the same
expression computed at runtime always agree.

*Against floor division (Python):* nicer for modular arithmetic, but costs an emitted
correction on every division.

### `orelse` tests a nullable union's tag

Never "if truthy". If the left type cannot contain `null` — a bare `i64` — then
`A orelse B` is always `A`, **including when A is `0`**.

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
which is fine because the passes are cheap. The stdlib signatures do not depend on the
choice, so fusion can arrive later against an IR with zero change to any program. What
could *not* arrive later is the permission: code that relied on "every `f`, then every
`p`" would break under fusion, so the ordering is fixed here before anyone can write
that code.

If you need one stage's effects fully sequenced before the next, that is a `for` loop,
not a pipeline. Pipelines are for transforming values, not for ordering effects.

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

### `throws` is unconstrained; `Error` is required only at the top

`throws T` accepts any type: `throws any`, `throws :ok`, `throws Person`, `throws bool`.
A `throws` clause is a claim about what a function can fail with, not a claim that the
failure is presentable. `throws E ... where E: Error` covers generic propagation and
already works, because the clause resolves in the function's rigid scope.

The `Error` bound applies at exactly one place: an error escaping `main`. That is the only
point where the language must render something it did not author, so it is the only point
that may demand an interface. A non-`Error` reaching it is a compile error telling you to
catch it or implement `Error`.

### `main` does not throw `any`

`main`'s implicit `throws Error` needed a type, and `Error` is a protocol -- a bound, not a
type -- so ⊤ was substituted. That cost two things. It boxed the error path; and because
everything is a subtype of ⊤, it silently switched off the check that a thrown value is an
error at all, so `throw 42` from `main` compiled.

`main`'s error channel is therefore **not a type**. It is a rule, checked per throw site.
No existential is needed to enforce it: at every escape point the concrete type is
statically known (or is a concrete union), so `message` is a direct call and a union is a
tag switch. There is no vtable, no box, and nothing named `Report`.

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
