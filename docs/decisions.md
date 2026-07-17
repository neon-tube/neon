# Decisions

The corpus (`tests/lang/`) says **what** the language does. This says **why**, and records
what was chosen against.

The old repo is a graveyard: a prior implementation, kept for reference. Its `tasks/6xx`
files hold earlier decisions and are **not** authoritative — several are contradicted
below. Read them for context, never as the answer, and do not edit them.

---

## Recursive types: explicit `mu type`

    mu type A = :ok | List[A]

Well-formed iff all of:

1. Recursive references occur **beneath a structural constructor**. `mu type T = T | i64`
   is an error.

   A guard is a position **visible in the type**, in a covariant slot. Two forms qualify:
   - a **generic argument**: `List[A]`, `Map[str, A]`. This is what guards
     `mu type A = :ok | List[A]` — `A` is visible in the type expression, and List's
     representation is irrelevant (it is a compiler builtin with no fields at all).
   - a **declared field of a data constructor**: with `record Node { next: T | null }`,
     `mu type T = Node` is well-formed.

   An **opaque nominal atom does not qualify** — `opaque record Bytes {}` has neither a
   generic argument nor a visible field, so there is no position for a recursive
   occurrence to sit in. The test is visible type structure, not whether the checker can
   unfold the representation.

   **`opaque` is module-scoped, not absolute.** An opaque record's fields are visible
   inside its own module and to a single parent module; only beyond that is it an atom.
   So visibility — and therefore contractivity — is judged **where the `mu type` is
   declared**. `opaque record Rng { seed: i64 }` is a data constructor with a guardable
   field inside `std::rand`, and an unguardable atom outside it. The same `mu type` can
   be well-formed in one module and rejected in another; that is intended, not a wrinkle.
2. Recursive references occur **only in covariant positions**. A function parameter is
   contravariant and therefore excluded; a return is covariant and allowed. This subsumes
   the older "no recursion through function types" rule.
3. **No recursion beneath negation or difference.**
4. The alias expands **equi-recursively — no runtime wrapper.** `mu type A = :ok | List[A]`
   and its one-step unfolding are the same type: no fold/unfold, no tag, no allocation.

Self-recursion only for v1; mutual recursion is a clear "not yet supported" error.

**A `mu type` with no recursive occurrence is an error.** The binder asserts recursion; if
there is none, either the binder is wrong or the type is. `mu type A = i64 | str` must be
rejected, pointing at plain `type`.

*Against:* implicit recursion through plain `type` (the graveyard's `#502` decision —
"no `rec`/`mu` binder"). Reversed because the restrictions above are unusual enough to be
worth a visible keyword, and a typo that creates accidental recursion should be a plain
error, not a silently-recursive type. A plain `type` alias that turns out recursive is an
error.

**`mu newtype` is banned, and a `newtype` may not be recursive.** `newtype T = List[T]`
is an error. Recursion is `mu type`'s job; a `newtype` is a nominal wrapper and nothing
else. A recursive *nominal* type is what `record` is for — records already recurse.
Without this, `newtype` would silently acquire recursion through the same lazy
name-reference mechanism its definitions table already uses, which is how you get a
feature nobody designed.

**A nominal recursive record satisfies a structural μ-type — structurally.**
`record Node { next: Node | null }` satisfies `mu type T = { next: T | null }`. This
follows from `#207` (nominal satisfies structural) and costs nothing extra: the field is a
legal guard either way. It makes a structural μ-type a way to accept a whole *family* of
nominal recursive records without naming any of them — write the shape once, and every
list-like record fits it. Intended, not incidental.

## Atoms are singleton types

`:ok` is both a value and the type inhabited by exactly that value, so `:ok | :err | str`
is a union. Atoms already existed as values; this lifts them to the type level.

## No default arguments — optional parameters are anonymous records

    fn connect(host: str, opts: { timeout: i64 | null, retries: i64 | null }) -> Conn {
        let t = opts.timeout orelse 30
        ...
    }

    connect(host, { timeout: 5 })

**A missing field satisfies a nullable field**, so `connect(h, {})`,
`connect(h, { timeout: 5 })` and the full form all typecheck, in any key order.
Optionality rides on `T | null` and `orelse`; there is no optional-field syntax and no
`=` defaults.

*Against:* default arguments (the graveyard's `#609`,
`fn greet(name: str, punct: str = "!")`), and Elixir-style keyword lists with trailing
`key: value` sugar. Records already do the job with real types; a keyword list would be a
second, weaker way to say the same thing.

*Accepted cost:* the one-optional-arg case is heavier than `punct: str = "!"` was.

## Lookups `throws`; bracket indexing traps

    get(xs, i) -> T throws KeyError
    try? map::get(m, k) orelse 30          // easy path
    try map::get(m, k) catch (e) { ... }   // when the distinction matters
    xs[i]                                  // traps on out-of-bounds. no try, no orelse.

*Against:* `get -> T | null` (the graveyard's `#602`). That is **unimplementable**, not
just inconvenient: unions flatten and are idempotent, so for `List[i32 | null]`,

    get(xs, i) : (i32 | null) | null  ==  i32 | null

and "absent" is indistinguishable from "present, holds null". Rust escapes via nesting
(`Option<Option<T>>`); Neon's unions collapse. `throws` puts absence on a separate
channel, so nothing collapses. `try?` recovers the `orelse` ergonomics without infecting
the caller's signature, and the ambiguity reappears only where the programmer opts into it
by writing `try?` — which is a request to collapse the distinction.

`[]` traps rather than throws because a bounds violation is a bug, not a recoverable
condition, and a checked throw would force `try xs[i]` on every element access. Trapping
is already the language's answer for `/` by zero.

## Indices are `i64`, and there is no negative indexing

    fn get[T](list: List[T], i: i64) throws IndexError -> T
    list::last(xs)              // not xs[-1]

*Against `usize`:* the common index bug is a computed `i - 1` at `i = 0`, not a literal
`xs[-1]`. With `i64` that traps as `index out of bounds: -1` and you see it instantly;
with `usize` and wrap-on-overflow it traps as `index out of bounds: 18446744073709551615`
and you have to decode it. Making `usize` good would need trapping subtraction — an
exception to the integer semantics, which say `+ - *` wrap, full stop. `usize` is also
`size_t` leaking through the API: lists are values here, indices are just numbers, and
`i64` loses no range (a list cannot exceed `i64::MAX` elements). `usize` would make
`xs[-1]` a compile error; that is the one thing given up, and a *literal* negative index
dies on first run anyway.

*Against Python-style negative indexing:* it converts that same `i - 1` bug from a trap
into a **silent wrong answer** — reading the last element instead of failing. That is the
bug class this rewrite exists to delete (`m[k]` returning zeros, dropped match guards,
enums matching the first arm). It also puts an `if i < 0 { i += len }` branch on every
element read, in the hottest loop, against a <2× C target. Python can afford both; it is
dynamically typed and already far from C. `list::last(xs)` is clearer, throws honestly,
and costs nothing.

## Abnormal termination exits 101, on stderr

Every abnormal exit — a trap (`/` by zero, index out of bounds), a `try!` panic, or an
error reaching `main` uncaught — prints to **stderr** and exits **101**.

*Why not 1:* a program can exit 1 deliberately via `std::exit(1)`, and 1 is the
conventional generic failure code besides. If traps also exit 1, nothing — not a test, not
a shell script, not a CI job — can tell a program that *chose* to fail from one that
*died*. Rust picked 101 for exactly this reason: it does not collide with codes a program
plausibly returns itself.

## `main` returns `()`, implicitly `throws Error`, and cannot say otherwise

    fn main() { ... }                           // the only form
    fn main() -> i64 { ... }                    // error: main's return type is fixed
    fn main() throws IoError { ... }            // error: main's throws clause is fixed

`main` returns `()`. The runtime wraps it to exit **0**. Both halves of its signature are
fixed: it cannot declare a return type and it cannot declare a `throws` clause — not
narrowed, not widened, not restated.

Other exit codes come from `std::exit(n)`, not from returning one. A return value would be
a second way to say what `exit` already says, and it would only work from `main`.

`main` carries an implicit `throws Error` that is never written and cannot be changed —
not narrowed, not widened, not restated. Since every error record implements `Error`, any
error propagates to `main`, so a bare `try foo()` in `main` always compiles without
declaring anything.

An error reaching the top prints the error's type and `Error::message()` and exits
non-zero. That is the only place an error may go unhandled: `main` is the catch-all, and
the runtime is its `catch`.

This is what makes `try` usable at the top without ceremony, and it means "requires a
compatible enclosing `throws`" is only a real constraint in *non*-`main` functions.

## `try` accepts a block

    try { a(); b() } catch (e) { ... }

The graveyard's AGENTS.md says "no block-level `try` — expression-level only". That
described the old implementation's limitation, not a decision.

## String interpolation is `#{expr}`

    "count: #{n}, json: { \"literal\": true }"
    "#{Point { x: 1, y: 2 }}"        // brace-matched; record literals are fine
    "#{fmt::pad(x, 8)}"              // formatting is functions

*Against:* `{expr}`. `{` already means records and blocks, so `{}` in a string collides
with both the record literal and a `{{`/`}}` escape — `"{Point { x: 1 }}"` is genuinely
ambiguous. With `#{`, a bare `{` is always literal and needs no escaping; only `#{` is
special, and `\#{` escapes it. Ruby and Elixir landed here for the same reason.

**A hole holds any expression**, terminated by brace matching. Nested quotes are fine
(`#{f("b")}`) — the lexer already tracks string state.

**There are no format specs.** No `#{x:8.2}`; formatting is ordinary functions, and
`Display` does the rest. A colon cannot mark a spec anyway: `#{:ok}` is an atom, so the
colon appears in leading position, and `#{m[:key]}` puts one mid-expression. Reserving
`:` would mean deciding where the expression ends before the lexer knows — the rule would
be incoherent, not merely awkward.

## `|>` binds tighter than comparison

    x |> f() == 3        // ((x |> f()) == 3)

*Against:* the older table, where `|>` (30) sat below `==` (40), making that parse
`x |> (f() == 3)` — piping into a comparison, which can never be a valid pipe target. A
pipe is a call, and calls bind tighter than comparison.

## `let x = :ok` has type `:ok`

Atoms are singleton types, and the binding keeps the singleton. So:

    let x = :ok
    x = :err                     // error: :err is not a subtype of :ok
    let y: :ok | :err = :ok
    y = :err                     // fine

*Against:* widening to an `atom` supertype (mirroring `let x = 1` widening to `i64`). That
would discard the singleton precision that makes `:ok | :err` unions and exhaustiveness
work — which is the entire reason atoms are types. Annotating is required exactly where
you intend to rebind, which is where an annotation earns its keep.

## Integers: C11 truncation

The runtime targets **C11**, not C99.

- `/` truncates toward zero; `%` takes the dividend's sign: `-7 / 2 == -3`, `-7 % 2 == -1`.
  Matches the backend, so nothing extra is emitted.
- `INT64_MIN % -1` **traps**, alongside `INT64_MIN / -1`, even though the answer is
  mathematically 0. One rule, one check, and `/` and `%` never disagree about when they
  trap.

*Against:* floor division (Python), which is nicer for modular arithmetic but costs an
emitted correction on every division, against a <2× C target.

**`bsr` is type-driven**: arithmetic on signed types, logical on unsigned.

    -8 bsr 1   ==  -4        // i64: sign-extends. Same as -8 / 2.
    x bsr 1                  // u64: zero-fills

The type already knows which you meant. Logical shift on a signed value produces nonsense
(`-8` → `9223372036854775804`); arithmetic shift on an unsigned one is meaningless. Same
call Rust makes; Java needs two operators (`>>`, `>>>`) only because its types don't say.

**Codegen must guarantee this, not inherit it.** C11 §6.5.7p5 makes `>>` on a negative
signed value *implementation-defined*. GCC and Clang happen to do arithmetic, but relying
on that would break the rule that constant folding and runtime agree exactly — the same
shape as wrapping arithmetic, where the language defines the behaviour and `-fwrapv` has
to guarantee it.

## Inherited, unchanged

From the graveyard's `tasks/`, still authoritative — restated here only as an index:

- **Rebindable `let`**, sealed by-value closure captures, shadowing (`#601`). No `mut`.
- **Checked throws** with the `try` / `try?` / `try!` triad; single `catch` with an
  internal `match`; `Ok`/`Err` deleted (`#602`, as amended above).
- **`and` binds tighter than `or`** (`#603`).
- **`else` required when an `if` is consumed** (`#604`).
- **`str` is UTF-8 bytes** (`#609`).
- **`enum` deleted** — sum types are unions of records (`#610`). The compensating
  ergonomics (`#607` destructuring, `#205` record patterns) are follow-through, not
  extras: without them every payload arm costs an `is X` / `as X` pair.
- **A nominal record satisfies a structural parameter** (`#207`).
- **`Type::Erased` only where the user wrote `any`** (`#406`).
- **Integer semantics**: `+ - *` wrap; `/` `%` trap on zero and `INT64_MIN / -1`;
  `bsl`/`bsr` mask the shift amount. Constant folding agrees with runtime exactly.
- **`orelse` tests a nullable union's tag**, never "if truthy". `0 orelse d` is `0`.
