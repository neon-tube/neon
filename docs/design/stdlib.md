# Design: the standard library

**Status:** built. `stdlib/` holds eleven `.neon` files, they are loaded on every
compilation, and `tests/lang/` exercises them. Each module's own header carries its
reasoning; this file covers what is true across all of them.

## Delivery: on disk, path-mapped

`stdlib/std/io.neon` **is** the module `std::io`. There is no `mod std { mod io { } }`
wrapper — the loader walks `stdlib/`, derives the module prefix from the path, and declares
every stdlib file before the user's module (`Env::build_with`, `cli/src/stdlib.rs`,
`cli/src/frontend.rs`).

The source is **not embedded in the binary**, and that is the load-bearing choice. An LSP
resolving go-to-definition into `println` has to open an actual file and show a span in it.
Embedding would make every stdlib location synthetic. `neon sysroot --stdlib` exists so
tools can ask the toolchain where the files are. See `docs/decisions.md` §"The runtime and
stdlib are data the toolchain ships".

The phases stay global and ordered: every module is declared before any body is resolved,
so a program may name a stdlib type and a stdlib function may name a program type. The
stdlib is numbered first so `ExprId`s across the whole compilation are unique and one
`TypecheckResult` covers both — stdlib bodies are real Neon and are lowered like any other.

## One way to turn a value into a string

    io::println("total: #{n}")     // sugar
    let s = to_string(n);          // explicit — the same mechanism

`Display` declares `to_string(v: T) -> str`, and `#{x}` desugars to `to_string(x)`.
One mechanism, two syntaxes. There is no `string::int_to_str`: a monomorphic converter can
never cover a user's record — that needs `Display` regardless — so keeping one would mean
two mechanisms forever, and the corpus would teach whichever one it showed.

The prelude ships `impl Display` for `i64`, `f64`, `bool` and `str`, each an `@pure @native`
one-liner.

`string::to_int` survives, because parsing is not stringifying: it can fail, and it throws
`ParseError`. The pair is deliberately asymmetric — `to_string` is total, `to_int` is
partial.

Formatting beyond that is functions, not format-string syntax: `"#{fmt::pad(name, 8)}"`.
One mechanism, and the knobs are in a place that can be named and tested rather than in a
mini-language inside a string.

## Names say what they count

`string::byte_len` counts **bytes**, and says so. A name is where a surprise belongs; a
comment on the declaration is not read at the call site. `list::len` and `map::len` keep
`len`, because elements are the only unit those could mean. `fmt`'s widths are bytes for
the same reason.

`string::char_count` still does not exist. Nothing has needed it.

## The prelude

Interpolation is syntax and desugars to a protocol call, so without a prelude every file
containing a string hole would need an import before a language feature worked. The rule
is: **if you can write it without naming it, it is in the prelude.**

`stdlib/prelude.neon` holds, with its own reasoning for each:

    Display              `#{x}` desugars to `to_string(x)`
    Error                what `main`'s implicit channel demands of anything that escapes
    marker Ord           the `where T: Ord` bound
    List, Map            `record_repr` still matches these two names literally
    range                `for i in range(a, b)`; moving it would put an import in front
                         of every counted loop
    IndexError           thrown by `std::string` and `std::collections::list` both, so it
                         has no single owner
    Ordering             returned by the `_by` callbacks `std::cmp` and `list` both take
    impl Display         for `i64`, `f64`, `bool`, `str`

`io::println` still takes `use std::io` — it is a function, not syntax.

`List` and `Map` are in the prelude because the backend matches those names literally, not
because syntax needs them. `@runtime` makes moving them out possible, and it is still
unbuilt; doing so also removes the prelude-vs-root collision forcing an exception in
`check.rs::opacity_permits`.

## Comparison is structural, not dispatched

`==` and `<` are primitives the backend expands per type. There is no `Eq` protocol and no
`Ord` protocol to implement. `==` compares by content on every type but a closure; `<`
orders lexicographically and is total *within* a type, never across one. Order is
infectious: a record is ordered when every field is, `List[T]` when `T` is; an atom, `null`,
a `Map`, a closure, a union and a self-referencing record are not.

A generic says `where T: Ord` — a **marker**, a bound with no methods, answered from the
type's structure. `Ordering` survives as the return type for the `_by` functions
(`cmp::max_by`, `list::sort_by`), which take the comparison as an argument and need no bound
at all. See "Comparison is structural" and "Markers" in `docs/decisions.md`.

*Was known-broken:* `env.rs::satisfies_marker` matched the bare protocol name `"Ord"`, so a
user `marker Ord` in any module used to inherit the built-in rule. **Fixed** — `is_known_marker`
and `satisfies_marker` test identity against the prelude's reserved module path, so a user's
`marker Ord` is "no rule for marker `Ord`" like any other name the compiler does not know
(`protocols/a_user_marker_named_ord_is_not_the_marker.neon`).

## `Error` is decoupled from `Display`

`protocol Error for T { fn message(v: T) -> str }`, and it is deliberately not
`protocol Error for T where T: Display`. `to_string` answers "how does this value render"
(`"Alice"`); `message` answers "what went wrong" (`"failed to load user Alice"`). A type can
be both a value and an error without the two answers fighting over one method. The
supertrait version also declared no methods of its own, so it was literally "`Display`, but
you also have to write an `impl`". See `docs/design/errors.md` and `docs/decisions.md`
§"`Error` owns `message`; `Display` is a separate concern".

In practice most stdlib error records implement both and both read the same field —
`IndexError`, `ParseError`, `KeyError`, `IoError`. That is the two questions happening to
have one answer, not the two protocols being one.

## Iteration and transformation

`for x in xs` is a **built-in index loop over `List`**, not a protocol. It is most of the
iteration in most programs and it must be a C loop over a contiguous buffer. It is not
extensible to user containers.

Transformation is eager and lives on an HKT protocol, in `std::collections::list`:

    protocol Mappable for C[_] {
        fn map[T, U](c: C[T], f: (T) -> U) -> C[U]
        fn filter[T](c: C[T], keep: (T) -> bool) -> C[T]
        fn fold[T, A](c: C[T], init: A, step: (A, T) -> A) -> A
    }

    impl Mappable for List { ... }

`Map[K, V]` does not fit `C[_]` — a two-parameter type is not a functor over one — so it
iterates through `map::keys`/`map::values` first. There is no `Iterator` type and no closure
streams: an arrow-typed `Iter[T] = () -> (T, Iter[T])` boxes a closure per element, which is
strictly worse than eager plus `rc == 1` in-place reuse on both allocations and indirect
calls. Pipeline effect order is interleaved by definition (`docs/decisions.md`), which
reserves fusion for later with no purity tracking and no signature change.

The impl is not an orphan: `List` is declared in the prelude, but this module owns the
protocol, and owning either end is enough.

Still open: infinite sources (`iterate(0, f)`), which cannot be a `List` and are the one
genuine case a lazy type would serve. Deferred until something needs it.

## Iolists, and why immutable strings do not make output quadratic

`std::fs` declares `mu type IoList = str | Bytes | List[IoList]`. It is a *tree*, not a
buffer: nesting a list inside a list is a pointer, so building one copies nothing and
concatenates nothing. `write_all` flattens it to the leaves' views and hands them to a
single `writev` as an `iovec` array, so the payload bytes never move.

This is Erlang's iodata, and it is the answer to the obvious objection to immutable strings:
appending in a loop would be O(n²) if output meant concatenation. It does not. Build the
tree, write once.

`Bytes` is `newtype Bytes = str` — same representation, nominal distinction only, so a PNG
is not something you would call `to_upper` on. Cast in either direction with `as`.
`tests/lang/collections/iolist_and_files.neon` pins the whole path.

## Surface

Module headers are the reference. The shape:

    prelude              Ordering, range, List, Map, marker Ord, Display, Error,
                         IndexError, Display impls for the primitives
    std::io              println, print, eprintln
    std::string          byte_len, concat, slice, char_at, to_int, join, find, contains,
                         starts_with, ends_with, to_upper, to_lower, repeat, is_empty,
                         split, lines, trim, trim_start, trim_end, trim_end_of, replace,
                         ParseError
    std::collections::list
                         Mappable (map/filter/fold), new, new_with_capacity, len, get, set,
                         push, concat, sort, sort_by, merge, is_empty, first, last,
                         contains, index_of, reverse, slice, sum
    std::collections::map
                         new, len, get, get_or, set, contains, remove, keys, values,
                         is_empty, KeyError
    std::fs              File, IoError, Bytes, IoList, open, create, open_append, close,
                         fd_of, read_all, write_all, flatten, exists, remove,
                         read, read_bytes, read_lines, write, append
    std::path            is_absolute, is_relative, components, join, parent, file_name,
                         stem, extension, with_extension, normalize, starts_with
    std::resource        Resource, ReleasedError, new, get, release, take, is_live, using
    std::math            sqrt, pow, floor, ceil, round, abs, is_nan, is_infinite,
                         abs_int, to_f64, to_int
    std::cmp             max, min, max_by, min_by
    std::fmt             pad, pad_left, pad_with, fixed

There is **no `std::exit`**, contrary to earlier drafts of this file. Abnormal termination
is `neon_trap`, which `_exit`s 101 on stderr (`docs/decisions.md`).

Every collection operation returns a new collection: these are values, which is what made
covariance sound. The copy is not always paid for — a uniquely owned list or map (`rc == 1`)
is updated in place, so `push` in a loop is amortised O(1) *as long as the old binding is
not kept*. Hold on to the input as well and every `push` copies, which turns that loop
quadratic. It is the one performance cliff in the collections worth knowing about.

## Annotations the stdlib relies on

Three, all stdlib-only. `docs/design/annotations.md` is the reference; the stdlib-specific
part:

- **`@native("symbol")`** — no body; the body *is* the runtime symbol. `expand.rs` checks
  the argument is present and the body is absent. **Nothing checks that a symbol exists.**
  A misspelled or unimplemented native is a *link* error, not a compile error, and the
  checker has nothing to object to.
- **`@pure`** — only valid on an `@native`; a Neon body's purity is inferred. Absence means
  effectful, which is the safe direction: forgetting it costs an optimisation, wrongly
  claiming it deletes real work. The analysis it replaced guessed from the symbol's spelling
  and defaulted to pure, which silently removed a resource construction along with the
  cleanup it existed to schedule.
- **`@runtime("neon_type")`** on a fieldless record — the backend represents it as a pointer
  to that C type. This is what lets `Resource` be declared in a stdlib module instead of
  being a name the compiler recognises.

## Why so much of the stdlib is Neon rather than C

A recurring reason, worth stating once: **a native cannot construct a program-specific
layout.** It cannot build a `List[T]` without the element's value-witness, which codegen
generates per program; it cannot build the tagged result a throwing function returns; it
cannot construct an error record.

So the pattern throughout is a thin `internal mod raw` of unchecked natives with the check,
the error and the union written in Neon above it — `list::get`, `string::slice`,
`map::get`, `resource::release`, all the same shape. `range`, `split`, `lines` and the
`std::path` module are pure Neon for the same reason. Less C, not more.

## Known-broken

- **`std::io`'s header is wrong.** It claims only `println` is implemented and that
  `print`/`eprintln` name symbols the runtime does not define. All three are defined in
  `runtime/src/io.c`. The comment is stale; the code is fine.
- **`fs::fail` and `fs::collect` are public** but read as helpers of `fs::open`/`fs::flatten`.
  Neither is in an `internal mod`. Probably an oversight.
- ~~**Stdlib diagnostics render against the user's file at a fabricated location.**~~
  **Fixed** — `TypeError` carries `module`, so an error in `std/io.neon` renders against
  `std/io.neon`.
- `tests/lang/collections/stdlib_breadth.neon` is the closest thing to a surface test; there
  is no per-function coverage guarantee.
