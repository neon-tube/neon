# Design: resources and cleanup

Status: **designed, not implemented** (2026-07-19). Nothing below exists in the compiler
yet; `std::fs`'s `File` is currently a compiler-known name in `record_repr`, which this
replaces.

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

    // std/resource.neon
    @runtime("neon_resource")
    opaque record Resource[T, E] {}

A refcounted runtime object holding a payload, a cleanup function, and an armed flag.

It is a **library type in a stdlib module**, not a language construct: `@runtime` is the
only thing the compiler contributes, and it *replaces* the hardcoded name table in
`record_repr` rather than adding to it. `List` and `Map` carry the same annotation, so the
special-case count goes from three to zero and `File` stops being magic. `@runtime` is
stdlib-only, like markers — it names a C type the backend must know.

Two rejected shapes, for the record:

- **`resource record File { fd: i64 }`** — a declaration modifier. Forced and correct, but
  a modifier that silently changes a type's representation is too large a consequence to
  hang on a keyword the reader may not know.
- **A guard *field*** (`record File { fd: i64, guard: Resource }`). Natural field access and
  no wrapper, but the payload stays reachable independently, so use-after-close is silent —
  and nothing forces the guard to be there at all. It prevents leaking without preventing
  use-after-release, which is half the value.

## API

    fn new[T, E](payload: T, cleanup: (T) throws E -> ()) -> Resource[T, E]
    fn get[T, E](r: Resource[T, E]) throws ReleasedError -> T
    fn release[T, E](r: Resource[T, E]) throws E
    fn take[T, E](r: Resource[T, E]) -> T | null
    fn is_live[T, E](r: Resource[T, E]) -> bool

`E` is the error the cleanup may throw, carried on the arrow type (`(T) throws E -> ()`,
the shape `types/arrow_type_throws.neon` pins). Encoding it as `throws` rather than a
returned union buys three things:

- `release` composes with `try`/`catch` like anything else that fails.
- Infallible cleanup is `Resource[T, never]`, whose `release` throws `never`, so the caller
  needs no `try` at all. A returned `E | ()` cannot express that.
- **Double release needs no sentinel and no cached result.** `release` disarms first; if it
  was already disarmed there is nothing to run and it returns normally. Idempotence falls
  out of the encoding instead of being engineered.

`get` throwing is what turns **use-after-release into a diagnosable error** rather than a
read of a stale descriptor. That is something neither Rust (which prevents it with
borrowck) nor Swift (which does not prevent it) offers here, and it costs one flag check on
operations that were about to make a syscall.

`release` is an ordinary Neon function, not a native: a native cannot build the tagged
result a throwing function returns. It calls two small natives — disarm-and-take, and
fetch-the-cleanup — and does the calling itself, so `throws` works normally. Less C, not
more.

## Shape for `std::fs`

    opaque record File { r: Resource[i64, IoError] }

`File` *holds* a resource rather than being a newtype over one: a newtype costs an
`as Resource[i64, IoError]` cast in every accessor, and holding one composes if something
ever needs two. `File` stays an ordinary inline record whose single field is refcounted, so
copying it retains, and `Resource` never appears in a user-facing type.

    fn close(f: File) throws IoError {
        try resource::release(f.r)
    }

    fn read_all(f: File) throws IoError -> str {
        let fd = try resource::get(f.r)      // throws if already closed
        ...
    }

## When cleanup runs

| situation | runs? |
| --- | --- |
| last use of the last reference | **yes** — the normal case, and *earlier* than scope end |
| leaving via `throw` | **yes** — `throw` is an ordinary return path |
| held in a `List`, a record, or a closure | yes, when that dies |
| explicit `release` | yes, immediately, and the caller sees `E` |
| reachable only through a cycle | yes, but when the collector runs — **not prompt** |
| `neon_trap` / `_exit` | **no** — no unwinding, by design |

Two rules worth stating outright, because both surprise:

**Only the explicit path can observe failure.** Drop has no error channel, so automatic
cleanup discards `E`. This is why `close` exists at all when the resource closes itself —
a question Rust never answered well, since `File` there has no `close` and observing a
close error is genuinely awkward.

**There is no ordering guarantee between independent resources.** Scope-based destruction
gives LIFO; last-use ARC gives "whenever each one's last use was", which can interleave. If
two resources must be torn down in order, one has to hold the other.

## What the compiler owes

- `@runtime` on a record → a runtime-pointer repr, replacing the name table in
  `record_repr`.
- `resource::new` is a codegen-assisted native (it needs `T`'s value-witness for the
  payload), in the same category as `neon_list_new`.
- A **monomorphic cleanup adapter** per instantiation, with the fixed signature
  `void(neon_header*, void*)`, so the runtime can call cleanup blind. The adapter knows `T`
  and `E`; it loads the payload, calls the closure, inspects the tagged result and
  **releases the error rather than propagating it**. Forgetting that release leaks on the
  *automatic* path only — invisible to the explicit path, which is exactly the shape of bug
  that ships. `emit_thunks` is the precedent.
- `get` uses the native out-parameter convention (`-> (bool, T)` compiling to
  `bool f(neon_resource*, void* out)`), which landed 2026-07-19.

## Resurrection

A cleanup that stores its own resource somewhere reachable would take the count `0 -> 1`
after the runtime decided to free. It is **unreachable by construction** here, for two
independent reasons:

- Cleanup receives the *payload*, not the resource, so it has no reference to store. For
  the payload to contain the resource there would have to be a cycle, in which case the
  count never reached zero.
- There is nowhere to put it. The closure is constructed *before* the resource exists, so
  it cannot capture it; captures are by value and sealed
  (`closures/capture_is_by_value_and_sealed.neon`); records are immutable; there are no
  mutable globals; and cleanup's return value is discarded on the drop path.

Immutability closes the hole that makes resurrection a real hazard in Rust and Swift. The
drop should still assert `rc == 0` afterwards: one comparison on a path that just made a
syscall, and it fails loudly if the language ever grows mutable shared state.

## Scope-lifetime resources: `using`

A lock guard's last use is the `lock` call itself, so under last-use ARC it releases *before*
the section it was meant to protect. The fix needs no language change -- only a function that
touches the resource on the far side of the body:

    fn using[T, E, R](r: Resource[T, E], body: (T) -> R) throws E -> R {
        let out = body(try get(r))
        try release(r)                  // <- the last use of `r` is HERE
        out
    }

    using(lock::new(m), (l) => {
        // the lock is held for exactly this region
    })

Because `r` is used *after* `body(...)` returns, the refcount pass must keep it alive across
the whole call. That is the entire trick: the combinator owns a region, and the region is
visible as a lambda rather than implied by a brace.

The failure path is right for free. If `body` throws, `release` never runs explicitly, but
ARC releases `r` on the throwing edge and cleanup fires anyway -- so the normal path observes
the cleanup error and the throwing path still cleans up silently, which is the two-path rule
already stated above.

This is deliberately **not** `defer`: nothing is registered and nothing is deferred. The cost
is the ordinary one for a scope combinator -- the body is a lambda, so control flow cannot
cross it. See the `return` note below, which makes that worse than it should be.

## Open

- **`return` inside a lambda is unsound**, which this pattern runs straight into: the checker
  types `return` against the *enclosing function*, while lowering lifts the lambda and
  returns from the lambda. See `docs/finalpush.md`. Until it is fixed, a `using` body must not
  contain `return`.
- **Cycles.** A resource reachable only through a cycle closes when the collector runs.
  Every other path here is deterministic; this one is not, and it is the single guarantee
  this design cannot make.
