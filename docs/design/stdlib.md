# Design: the standard library

**Status:** proposed. Nothing built — there is not one `.neon` file outside `tests/`.

## Why this is being designed rather than ported

The corpus is the spec, and it currently specifies a language working around a bug
that no longer exists.

687 `io::println` calls; **289 of them wrap `string::int_to_str`**. 84 wrap
`string::concat`. Meanwhile `decisions.md` specifies `#{expr}` interpolation as a
headline feature and **7 of 201 files use it**.

That ratio is not taste. The graveyard's `ir/lower.rs` returned `Erased` from every
protocol call except `eq` — including every `to_string`. So `Display` did not work,
interpolation could not work (it needs Display), and the corpus routed around a broken
protocol system with a monomorphic escape hatch. Then 289 lines fossilised it.

The bug is fixed: dispatch returns the union of the applicable impls' returns, and
there is nowhere for `Erased` to enter. So the escape hatch has nothing left to escape.

`string::int_to_str` is a **codegen bug wearing a stdlib API's costume** — the same
shape as invariance, as `T | null` for lookups, as "no block-level try". This project
keeps finding those, and this is the last big one.

## One way to turn a value into a string

    io::println("total: #{n}")     // sugar
    let s = to_string(n);          // explicit — the same mechanism

`Display` declares `to_string(v: T) -> str`, and `#{x}` desugars to `to_string(x)`.
One mechanism, two syntaxes.

`string::int_to_str` and `string::float_to_str` are **deleted**, not renamed. A
monomorphic converter can never cover a user's record — that needs `Display` regardless
— so keeping one means two mechanisms forever, and the corpus would teach whichever one
it showed.

The protocol's method is `to_string`, not the corpus's current `display`. `display(x)`
reads as a noun; `to_string(x)` says what it does, and it is the name the sugar
desugars to.

`string::to_int` survives: parsing is not stringifying, it can fail, and it throws.
The pair is deliberately asymmetric — `to_string` is total, `to_int` is partial.

## Names say what they count

`string::len` counts **bytes**. The corpus pins this on purpose: `size("é")` is 2.

It is now **`string::byte_len`**. A name is where a surprise belongs; a comment on the
declaration is not read at the call site. `list::len` and `map::len` keep `len` because
elements are the only unit those could mean.

`string::char_count` can arrive when something needs it. Nothing does yet.

## The prelude

Interpolation is syntax, and it desugars to a protocol call. Without a prelude, every
file containing a string hole needs an import before a language feature works — the
tail wagging the dog.

So there is a prelude, and it is small: the protocols that **syntax** depends on.

    Display     `#{x}` desugars to `to_string(x)`
    Eq          `==` and `!=`
    Ord         `<`, `<=`, `>`, `>=`
    Error       `throws`, and what `catch` binds

Nothing else. `io::println` still takes `use std::io` — it is a function, not syntax.
The rule is: **if you can write it without naming it, it is in the prelude.**

## Surface

    std::io
      println(s: str)
      eprintln(s: str)
      print(s: str)

    std::string
      byte_len(s: str) -> i64
      concat(a: str, b: str) -> str
      slice(s: str, from: i64, to: i64) throws IndexError -> str
      char_at(s: str, i: i64) throws IndexError -> str
      to_int(s: str) throws ParseError -> i64
      join(parts: List[str], sep: str) -> str
      find(s: str, needle: str) -> i64 | null
      contains(s: str, needle: str) -> bool
      starts_with(s: str, p: str) -> bool
      ends_with(s: str, p: str) -> bool
      to_upper(s: str) -> str
      to_lower(s: str) -> str
      repeat(s: str, n: i64) -> str
      is_empty(s: str) -> bool

    std::collections::list
      new[T]() -> List[T]
      len[T](xs: List[T]) -> i64
      get[T](xs: List[T], i: i64) throws IndexError -> T
      set[T](xs: List[T], i: i64, v: T) throws IndexError -> List[T]
      push[T](xs: List[T], v: T) -> List[T]
      concat[T](a: List[T], b: List[T]) -> List[T]

    std::collections::map
      new[K, V]() -> Map[K, V]
      len[K, V](m: Map[K, V]) -> i64
      get[K, V](m: Map[K, V], k: K) throws KeyError -> V
      set[K, V](m: Map[K, V], k: K, v: V) -> Map[K, V]
      contains[K, V](m: Map[K, V], k: K) -> bool
      keys[K, V](m: Map[K, V]) -> List[K]
      values[K, V](m: Map[K, V]) -> List[V]

    std::exit(n: i64)

Every collection operation returns a new collection: these are values, which is what
made covariance sound.

## Open

- **Iteration.** Settled in shape, open in surface.

  `for x in xs` is a built-in index loop over `List`, not a protocol — it is most of the
  iteration in most programs and it must be a C loop over a contiguous buffer. It is not
  extensible to user containers in v1, and that is fine.

  Transformation is **eager, HKT `Mappable`**: `protocol Mappable for C[_]` with
  `map`/`filter`/`fold` returning a new `C`. `Map[K,V]` does not fit `C[_]` (wrong
  arity) and iterates via `map::values`/`map::entries` first — a two-parameter type is
  not a functor over one. No `Iterator` type, no closure streams, no associated types:
  an arrow-typed `Iter[T] = () -> (T, Iter[T])` boxes a closure per element, which is
  strictly worse than eager + `rc == 1` reuse on both allocations and indirect calls.

  Pipeline effect order is interleaved by definition (see `decisions.md`), which
  reserves fusion for later with no purity tracking and no signature change. v1 lowers
  eager per stage.

  Still open: the exact `Mappable` method set; whether `fold` is a method or a free
  function; infinite sources (`iterate(0, f)`), which cannot be a `List` and are the one
  genuine case a lazy type would serve — deferred until something needs it. **HKT
  dispatch is designed in `dispatch.md` but unbuilt**, and `Mappable` cannot be called
  until it exists — that, and first-class calls (`g(1)` on an arrow-typed local, which
  the checker currently rejects), are the two real blockers.
- **`Error` vs `Display`.** `Error` declares `message(e) -> str`; `Display` declares
  `to_string(v) -> str`. For an error those are the same string, and Neon has no
  protocol inheritance to say so. Either `Error: Display` needs designing, or `message`
  should go and errors should just be `Display`.
- **Does `==` dispatch?** The checker types it `bool` without dispatching. If `==` on a
  record means `Eq::eq`, that is a lowering of an operator to a protocol call and it
  needs specifying — including what `==` means with no impl.
- **`@native`.** Every leaf here bottoms out in the runtime. The annotation exists and
  the parser accepts a body-less `@native fn`; nothing checks that one has a native
  symbol behind it.
