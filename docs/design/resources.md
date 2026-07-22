# Design: resources and cleanup

Status: **implemented** (2026-07-19). `std::resource` is a real stdlib module,
`std::fs` is built on it, and `tests/lang/resources/` pins the behaviour.

This file is the *argument*. The module's own header
(`stdlib/std/resource.neon`) is the current, deliberately-written reference for the
API and is not repeated here; `docs/decisions.md` §"Cleanup is a value, not a keyword"
records the decision. Read those for what it does. Read this for why the shape is what
it is.

## The problem

Something has to close a file descriptor. Two paths must both work, and they must not
collide:

    let file = try fs::open("test.txt")
    // ...
    try fs::close(file)                  // explicit: I want to see the error

    let file = try fs::open("test.txt")
    // ...just stop using it            // managed: something closes it for me

Explicit close plus automatic cleanup double-closes unless the automatic half can be
disarmed — and a double close is worse than a leak, because descriptor numbers are reused
and the second close lands on someone else's file.

## Why cleanup needs a place to live

Neon is ARC with value semantics: records are copied freely and sharing is refcounted.
"The last reference dies" is not merely hard to detect for an inline record — it is
**undefined**, because copying one produces two independent values.

The refcount pass *does* give inline aggregates a last-use hook (`rc_parts_rec` releases a
record's counted fields), but it fires **once per copy**. For a `str` field that is correct,
because each copy was retained. For a user-supplied cleanup it means running it once per
copy — a double close by another name.

So cleanup needs either identity, or the absence of copying. Two languages, two answers:

| | discipline | destructor needs identity? |
| --- | --- | --- |
| Rust | affine; moves, and `Copy` + `Drop` is a hard error | no — drops on the stack |
| Swift | ARC, copyable values | **yes** — `deinit` on classes only |

Neon is Swift-shaped, so it gets Swift's answer. Linearity is the better theory but it
splits the type system into copyable and non-copyable, and every generic, container and
closure then has to say which it accepts. Swift itself only added noncopyable types after a
decade of the simpler rule, and for the cases where copying is *semantically* wrong — locks,
transactions, one-shot tokens. A file handle is not one of those.

## The type

    // stdlib/std/resource.neon
    @runtime("neon_resource") sealed opaque record Resource[T, E] {}

A refcounted runtime object holding a payload, a cleanup closure, and an armed flag
(`runtime/include/neon/resource.h`).

It is a **library type in a stdlib module**, not a language construct: `@runtime` is the
only thing the compiler contributes. That annotation replaces what used to be a literal
string match on `"File"` in `ir/repr.rs::record_repr`, and the annotation is now what
produces `Repr::Runtime { nominal, c_type, args }`. One caveat against the original claim
that "the special-case count goes from three to zero": `List` and `Map` are **still their
own `Repr` variants**, because their element reprs feed witness emission and so move
separately (`ir/repr.rs`, `Repr::Runtime` doc comment; `TODO.md` item 17 tracks the rest of
the untangling, including getting them out of the prelude). `File` did stop being magic.
`@runtime` is stdlib-only, like markers; see `docs/design/annotations.md`.

Two rejected shapes, for the record — with the first one revisited, because the shipped
design is closer to it than the original argument admitted:

- **`resource record File { fd: i64 }`** — a declaration modifier. Forced and correct, but
  a modifier that silently changes a type's representation is too large a consequence to
  hang on a keyword the reader may not know.
- **A guard *field*** (`record File { fd: i64, guard: Resource }`). Rejected because the
  payload stays reachable independently, so use-after-close is silent, and nothing forces
  the guard to be there.

  **What shipped is a guard field** — `opaque record File { r: Resource[i64, IoError] }` —
  and the objection is answered not by opacity but by *deleting the payload field*. The fd
  lives inside the resource and comes back only through `get`, which checks the flag. There
  is no second route to it, so there is nothing to use after close. Opacity is a second
  fence, not the argument.

  Opacity is now genuinely enforced (three routes closed: field read, literal, destructuring
  — `tests/lang/records/opaque_hides_its_contents.neon`) and module-path forgery is refused
  (`tests/lang/types/a_module_path_may_not_be_forged.neon`). But **it does not hold in
  general**: nominal identity is a bare name, so a second module declaring `record File`
  declares the *same type* and can build one. See `TODO.md` item 1, which names
  `std::fs`'s guard explicitly. The fence the shipped `File` leans on is the missing field,
  which survives that bug; the `opaque` marking does not.

## API

As shipped (`stdlib/std/resource.neon`):

    fn new[T, E](payload: T, cleanup: (T) throws E -> null) -> Resource[T, E]
    fn get[T, E](r: Resource[T, E]) throws ReleasedError -> T
    fn release[T, E](r: Resource[T, E]) throws E -> null
    fn take[T, E](r: Resource[T, E]) -> T | null
    fn is_live[T, E](r: Resource[T, E]) -> bool
    fn using[T, E, R](r: Resource[T, E], body: (T) -> R) throws E | ReleasedError -> R

Cleanup is `(T) throws E -> null`. **`()` is not a type in this language** — the unit type
is `null`, and earlier drafts of this file wrote `()`, which cost an implementer real time.
`throws E -> null` is the shape `tests/lang/types/arrow_type_throws.neon` and
`tests/lang/closures/generic_throws_parameter.neon` pin.

Encoding `E` as `throws` rather than a returned union buys three things:

- `release` composes with `try`/`catch` like anything else that fails.
- Infallible cleanup is `Resource[T, never]`, whose `release` throws `never`, so the caller
  needs no `try` at all. A returned `E | null` cannot express that. `never` is writable in
  source as of 2026-07-19 precisely for this case —
  `tests/lang/types/never_is_writable.neon` writes the annotation and calls `release` bare.
- **Double release needs no sentinel and no cached result.** `release` disarms first; if it
  was already disarmed there is nothing to run and it returns normally. Idempotence falls
  out of the encoding instead of being engineered.

`get` throwing is what turns **use-after-release into a diagnosable error** rather than a
read of a stale descriptor. That is something neither Rust (which prevents it with
borrowck) nor Swift (which does not prevent it) offers here, and it costs one flag check on
operations that were about to make a syscall.

`release` is an ordinary Neon function, not a native: a native cannot build the tagged
result a throwing function returns. It calls two small natives — `raw::disarm` and
`raw::cleanup` — and does the calling itself, so `throws` works normally. Less C, not more.

`take` disarms and hands the payload out *without* running cleanup, so the caller takes
responsibility for it. On the drop path the payload is moved out and the source slot
zeroed, so the release in `neon_resource_finish` cannot reach bytes whose ownership has
already gone — a scalar payload hides that bug and a `str` payload turns it into a
use-after-free, which is why `tests/lang/resources/resource_with_refcounted_payload.neon`
exists.

## Shape for `std::fs`

    opaque record File { r: resource::Resource[i64, IoError] }

Shipped, and the runtime's hand-written `neon_file` is gone: `runtime/src/file.c` now holds
only the `neon_io_*` natives, and the handle is a resource like any other.

`File` *holds* a resource rather than being a newtype over one: a newtype costs an
`as Resource[i64, IoError]` cast in every accessor, and holding one composes if something
ever needs two. `File` stays an ordinary inline record whose single field is refcounted, so
copying it retains, and `Resource` never appears in a user-facing type.

    fn close(f: File) throws IoError -> null {
        try resource::release(f.r);
        null
    }

    fn fd_of(f: File) throws IoError -> i64 {
        try resource::get(f.r) catch (e) {
            throw IoError { message: "this file is already closed" }
        }
    }

`fd_of` translating `ReleasedError` into `IoError` is the reason `Resource` stays out of
`std::fs`'s public error types. `tests/lang/collections/iolist_and_files.neon` runs the
whole path end to end.

## When cleanup runs

| situation | runs? |
| --- | --- |
| last use of the last reference | **yes** — the normal case, and *earlier* than scope end |
| leaving via `throw` | **yes** — `throw` is an ordinary return path |
| held in a `List`, a record, or a closure | yes, when that dies |
| explicit `release` | yes, immediately, and the caller sees `E` |
| reachable only through a cycle | yes, but when the collector runs — **not prompt** |
| `neon_trap` / `_exit` | **no** — no unwinding, by design |

`tests/lang/resources/resource_cleanup_runs_at_last_use.neon` pins the first row, including
the part that surprises: cleanup prints *before* the line after the construction, because
the last use is the construction. It also pins an optimiser property — constructing a
resource looks pure, so DCE would delete it and the cleanup with it if the native did not
count as effectful. `neon_resource_new` deliberately carries no `@pure`.

Two rules worth stating outright, because both surprise:

**Only the explicit path can observe failure.** Drop has no error channel, so automatic
cleanup discards `E`. This is why `close` exists at all when the resource closes itself —
a question Rust never answered well, since `File` there has no `close` and observing a
close error is genuinely awkward.

**There is no ordering guarantee between independent resources.** Scope-based destruction
gives LIFO; last-use ARC gives "whenever each one's last use was", which can interleave. If
two resources must be torn down in order, one has to hold the other.

## What the compiler contributes

- `@runtime` on a record → `Repr::Runtime`, replacing the name table in `record_repr`.
  `expand.rs::Runtime` rejects it on anything but a record and rejects a record with fields:
  the C type owns the layout, so a field here would describe one it does not have.
- `raw::new` is a codegen-assisted native (it needs `T`'s value-witness for the payload),
  in the same category as `neon_list_new`.
- A **monomorphic cleanup drop** per instantiation, reached through `header.drop` rather
  than a field of its own — `neon_alloc` already takes a per-object drop and one
  indirection is enough. The emitted drop knows `T` and `E`; it takes the payload if still
  armed, calls the closure, inspects the tagged result and **releases the error rather than
  propagating it**, then calls the shared `neon_resource_finish`. Forgetting that release
  leaks on the *automatic* path only — invisible to the explicit path, which is exactly the
  shape of bug that ships.
- `raw::get` and `raw::disarm` use the native out-parameter convention (`-> (bool, T)`
  compiling to `bool f(neon_resource*, void* out)`).

## Resurrection

A cleanup that stores its own resource somewhere reachable would take the count `0 -> 1`
after the runtime decided to free. It is **unreachable by construction** here, for two
independent reasons:

- Cleanup receives the *payload*, not the resource, so it has no reference to store. For
  the payload to contain the resource there would have to be a cycle, in which case the
  count never reached zero.
- There is nowhere to put it. The closure is constructed *before* the resource exists, so
  it cannot capture it; captures are by value and sealed
  (`tests/lang/closures/capture_is_by_value_and_sealed.neon`); records are immutable; there
  are no mutable globals; and cleanup's return value is discarded on the drop path.

Immutability closes the hole that makes resurrection a real hazard in Rust and Swift.

*Undocumented:* this file previously asked for an `rc == 0` assertion after the drop. No
such assertion is in `runtime/src/resource.c` today and nothing records whether it was
considered and dropped or simply not written.

## Scope-lifetime resources: `using`

A lock guard's last use is the `lock` call itself, so under last-use ARC it releases *before*
the section it was meant to protect. The fix needs no language change — only a function that
touches the resource on the far side of the body. Shipped, in `std::resource`:

    fn using[T, E, R](r: Resource[T, E], body: (T) -> R) throws E | ReleasedError -> R {
        let payload = try get(r);
        let out = body(payload);
        try release(r);                 // <- the last use of `r` is HERE
        out
    }

Two differences from the original sketch, both consequences of `get` being fallible: the
clause is `throws E | ReleasedError`, not `throws E`, and the payload is fetched once up
front rather than inside the call. `body` is `(T) -> R` — it does not throw, so a throwing
body needs its own `try` inside the lambda.

Because `r` is used *after* `body(...)` returns, the refcount pass must keep it alive across
the whole call. That is the entire trick: the combinator owns a region, and the region is
visible as a lambda rather than implied by a brace.

The failure path is right for free. If `body` throws, `release` never runs explicitly, but
ARC releases `r` on the throwing edge and cleanup fires anyway — so the normal path observes
the cleanup error and the throwing path still cleans up silently, which is the two-path rule
already stated above.

This is deliberately **not** `defer`: nothing is registered and nothing is deferred. The cost
is the ordinary one for a scope combinator — the body is a lambda, so control flow cannot
cross it.

*No corpus file exercises `using`.* It type-checks and is written against shipped
primitives, but nothing in `tests/lang/resources/` calls it, so it is unproven at runtime.

## Open

- **Cycles.** A resource reachable only through a cycle closes when the collector runs.
  Every other path here is deterministic; this one is not, and it is the single guarantee
  this design cannot make.
- **Opacity is not identity.** See `TODO.md` item 1. The shipped `File` does not depend on
  it (there is no payload field to reach), but any *other* resource wrapper that keeps a
  payload alongside its guard would.

Throwing closures — listed here for months as the unmet prerequisite — landed. A lambda can
throw (`tests/lang/closures/throwing_lambda.neon`), a named throwing function is usable as a
value (`throwing_fn_as_value.neon`), and a throwing arrow survives a generic slot
(`generic_throws_parameter.neon`), which is what `resource::new` needs.
