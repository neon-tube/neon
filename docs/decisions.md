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

   A guard is a position **visible in the type**, in a covariant slot. Two forms qualify:
   - a **generic argument**: `List[A]`, `Map[str, A]`. This is what guards
     `mu type A = :ok | List[A]` — `A` is visible in the type expression, and List's
     representation is irrelevant.
   - a **declared field of a data constructor**: with `record Node { next: T | null }`,
     `mu type T = Node` is well-formed.

   An **opaque nominal atom does not qualify** — `opaque record Bytes {}` has neither a
   generic argument nor a visible field, so there is no position for a recursive
   occurrence to sit in. The test is visible type structure, not whether the checker can
   unfold the representation.

   **`opaque` is module-scoped, not absolute.** An opaque record's fields are visible
   inside its own module and to a single parent module; only beyond that is it an atom. So
   contractivity is judged **where the `mu type` is declared**: `opaque record Rng { seed: i64 }`
   is a data constructor with a guardable field inside `std::rand`, and an unguardable atom
   outside it. The same `mu type` can be well-formed in one module and rejected in another.

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
