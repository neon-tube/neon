# Errors

`throw` / `try` / `catch`, the `Error` protocol, and what a failure carries. The rationale
for each choice is in `docs/decisions.md` §Errors; this is the shape. `tests/lang/errors/`
is the specification.

## `Error`

    protocol Error for T {
        fn message(v: T) -> str
    }

Declared in `stdlib/prelude.neon`. `message` is the one thing an error knows about itself.

It is deliberately *not* `Display`, and not `protocol Error for T where T: Display` either.
`to_string` answers "how does this value render" (`"Alice"`); `message` answers "what went
wrong" (`"failed to load user Alice"`). A type can be both a value and an error without the
two answers fighting over one method. The supertrait version also declared no methods of its
own, so it was literally "`Display`, but you also have to write an `impl`" — a marker and a
capability doing unrelated work under one name.

    record IoError { message: str }

    impl Display for IoError { fn to_string(v: IoError) -> str { v.message } }
    impl Error for IoError { fn message(v: IoError) -> str { v.message } }

Most stdlib errors implement both and both read the same field. That is the two questions
happening to have one answer here, not the two protocols being one.

Everything else people reach for — context, a stacktrace — is a property of *where it was
thrown*, which the value cannot know. Those belong to the throw, and a default method body
could not supply them either: a default can compute from fields the type already has, but it
cannot create storage.

## `throws`

A `throws` clause takes **any type** — it is a claim about what a function can fail with,
not a claim that the failure is presentable:

    fn a() throws any
    fn b() throws :ok
    fn c() throws Person
    fn d() throws IoError | ParseError

The corpus pins atoms and atom unions (`types/throws_binds_below_the_arrow.neon`), record
types and record unions (`errors/`), and a type variable
(`closures/generic_throws_parameter.neon`). `throws any` is decided in `docs/decisions.md`
§"`throws` is unconstrained" but has no corpus file of its own.

`throws E ... where E: Error` covers generic propagation, because the clause resolves in the
function's rigid scope:

    fn retry[E](f: (i64) throws E -> i64, x: i64) throws E -> i64 where E: Error

Note the position: `throws` sits between the parameter list and `->`, the same order a `fn`
declaration uses. `tests/lang/types/arrow_type_throws.neon` pins it, along with the variance:
`throws` is covariant like the return, and an absent clause is `never`, not `any`. `never` is
the only sound default — `any` would read as "may throw anything", making every arrow a
supertype of every other.

`never` is writable in source as of 2026-07-19
(`tests/lang/types/never_is_writable.neon`). The motivating case is `Resource[T, never]`,
whose `release` throws `never` and so needs no `try` at all. A type the compiler infers and
prints but the user cannot spell is a hole: any annotation naming what was inferred becomes
inexpressible. It is empty, so a value position annotated `never` rejects every value there
is (`never_has_no_values.neon`).

## The one place `Error` is required

An error escaping `main`. That is the only point where the language must render something it
did not author, so it is the only point that demands the interface. Anything else is a
compile error:

    `i64` escapes `main`, which must report it, but it does not implement `Error`
    -- catch it, or give it an `impl Error` with a `message`

`main`'s channel is therefore **not a type** — it is a rule, checked per throw site
(`Throws::ImplicitError` in `typecheck/check.rs`, applied when a root-module `fn main` has no
`throws` clause). No existential is needed to enforce it: at every escape point the concrete
type is statically known, or is a concrete union, so `message` is a direct call and a union
is a tag switch. There is no vtable, no box, and nothing named `Report`.

The check itself is `implements_error`: asking dispatch to resolve `message` for the thrown
type. That succeeds only when every value the type admits has an impl, which is what makes it
answer correctly for a union without a special case.

The clause is **fixed**, not merely implicit: writing one on `main` at all is an error, even
a true one (`errors/main_throws_clause_is_fixed.neon`,
`errors/main_throws_error_restated_fails.neon`). `main` throws `Error` and no program gets to
say it throws less.

This holds precisely as long as **protocols stay bounds and never become types**. The day
`List[Error]` or `fn log(e: Error)` is expressible, a value of unknown type must be carried
at run time and a real protocol object is required.

## `catch` binds the concrete type

    try { let b = go(true); } catch (e) {
        io::println(string::concat("caught: ", e.msg));
    }

`e` is the thrown record, not an erased error object, so its fields are readable directly.
That falls out of the same property as above: at the catch site the type is statically known.

The triad is `try` / `try?` / `try!`. `try?` produces `T | null` and composes with `orelse`
(`errors/try_soften_orelse.neon`); `try!` asserts and traps. A bare call to a throwing
function without any of them is a diagnostic (`errors/bare_throwing_call_fails.neon`), and a
`try` inside a function with no compatible `throws` is too
(`errors/try_in_non_throws_fn_fails.neon`).

## What a throw carries

**As shipped, nothing beyond the error value.** A throwing function returns
`Union([ret, throws])` — a plain tagged union of the return type and the thrown type
(`ir/repr.rs`). A throwing closure's function returns the same, `throws` being part of the
calling convention rather than the layout.

Earlier drafts of this file described the slot as

    { tag, union { ok: T, err: { error: E, ... } } }

with the `...` reserved for a stacktrace and later for context. **That shape does not exist.**
It remains the intent — anything beyond the error value itself has to live somewhere the
error value cannot know about, and a protocol default cannot create storage — but the
inner struct is unbuilt, so treat the diagram as a plan, not a description.

## Diagnostics

`check_all` returns `(TypecheckResult, Vec<TypeError>)` and its error list is the only one a
caller has to read: it drains `Env::errors` (raised while resolving type annotations, during
the same walk) into the checker's own list, sorts them by span, and deduplicates on
`(span, kind)`. The dedup exists because a generic call checks each argument twice — once
while solving the callee's type parameters, then again under the solution — so anything
wrong inside an argument was reported twice.

*Was known-broken:* the sort was by **raw span offset across every module**, and one
`Renderer` holds one file, so a stdlib diagnostic rendered against the *user's* file at a
fabricated location. **Fixed** — `TypeError` carries `module`, whose doc records exactly this
failure, and a stdlib mistake renders against the stdlib source.

## Not yet

- **Stacktrace.** *Important, and the largest missing piece.* Nothing is implemented: no
  capture, and no slot in the error struct to put one in (see above). The value does not
  know where it was thrown, so it would be reached by a compiler-provided builtin rather
  than a protocol method.

  What it needs: **frame capture at the point of `throw`**, which the runtime cannot do
  today. Traps `_exit` with no unwinding by design (`docs/decisions.md`), so there are no
  frames to walk. Two routes, neither cheap:
  - DWARF unwinding (`backtrace()` / libunwind). Interacts with the `_exit`-on-trap and
    allocator decisions, and needs the C emitted with frame pointers.
  - A shadow stack the compiler maintains, paid for on every call. Predictable and
    portable; costs the hot path. Note this one does not care about frame pointers at all.

  *The frame-pointer conflict is settled (2026-07-19) and is no longer a blocker.*
  `opt-release` trims the frame pointer; a stacktrace needs it. They are mutually exclusive
  and the trace wins: `--stacktrace`, or `stacktrace = true` under `[build]` in
  `neon.toml`, suppresses `-fomit-frame-pointer` and passes `-fno-omit-frame-pointer`
  instead. Explicitly, because `-O3` omits frame pointers on most targets on its own, so
  merely dropping the flag would not have been enough —
  `stacktrace_and_frame_pointer_omission_are_exclusive` in `cli/src/buildcfg.rs` pins both
  halves. The switch exists and does nothing else yet.

  Opt-in beyond that is a `mode` concern: traces on in `debug`, off in `release`. With
  traces off the slot is not in the layout, so it costs nothing.

  Open: whether frames record argument values (much more useful, much more capture cost),
  and whether a rethrow extends the original trace or starts a new one.

- **Context.** Deliberately unresolved. Every syntax considered (postfix clause, frame-level
  statement, `@context` annotation, scoped block) was rejected on feel, and it is worth
  noting the open question underneath: *a good stacktrace may make hand-written context
  largely redundant* — `load(path="/etc/cfg") → open → permission denied` says what
  `"while loading config"` would have. Settle stacktrace first; context may shrink to
  the exceptional case or disappear.

- **`throws 1` / `throws "test" | false`.** Singleton types do not exist anywhere in the
  language — the kinds are base bits, atoms, records, tuples, arrows. Pinned deliberately;
  it is a type-system feature, not an error-system one.
