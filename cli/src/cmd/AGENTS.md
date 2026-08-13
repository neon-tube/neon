# Writing Neon

This is a guide for a coding agent writing Neon for the first time. Neon is a small,
statically-typed language that compiles to C. Read the gotchas — most bugs an agent writes
in Neon come from an assumption carried over from another language, not from the syntax.

## The one idea to hold onto

**Every value is immutable, and the compiler mutates in place when it proves nobody can
tell.** You write immutable code; the optimiser gives it the speed of mutable code. There is
no `mut` keyword and no aliasing to reason about. This one fact explains most of what follows.

## Syntax at a glance

```neon
use std::io
use std::list

// A record is a product type. Construct it by naming its fields.
record Point { x: i64, y: i64 }

// A union of records is a sum type. `enum` does not exist.
record Circle { r: i64 }
record Rect { w: i64, h: i64 }
type Shape = Circle | Rect

// The last expression in a block is its value: no `return` needed.
fn area(s: Shape) -> i64 {
    match s {
        is Circle => s.r * s.r * 3,   // this arm narrows `s` to Circle, so `s.r` is legal
        is Rect => s.w * s.h,
    }
}

fn main() {
    let p = Point { x: 1, y: 2 };     // `let` binds; statements end in `;`
    let total = area(Circle { r: 4 });
    io::println("area is #{total}");  // "#{expr}" interpolates into a string
}
```

- **Types**: `i64`, `f64`, `str`, `bool`; `List[T]`, `Map[K, V]`, `Set[T]`; tuples `(A, B)`;
  unions `A | B`; the optional `T | null`. `str` is UTF-8 bytes.
- **Atoms**: `:ok`, `:not_found` — a value that is its own name, compared by identity. Use
  them for small closed sets of tags.
- **Runes**: `'a'` is a single character; `"a"` is a string.
- **Interpolation**: `"#{expr}"` renders `expr` through `Display`. `"#{{"` and `"}}"` are
  literal braces.
- **Pipe**: `x |> f(y)` is `f(x, y)` — reads left to right for call chains.
- **Comments**: `//` line, `/* */` block, `///` doc (surfaced by `neon doc`).

## The gotchas (read these)

### 1. There is no `mut`. Rebind instead.

`let` can rebind a name, and `x = e` reassigns one. There is no mutable-variable keyword,
because values are immutable and the compiler earns the mutation.

```neon
let n = 0;
n = n + 1;          // reassign
let n = "now a str"; // rebind, even to a new type
```

### 2. Build collections by rebinding — and don't hold the old binding.

`list::push`/`list::set`/`map::set` return a **new** collection; they never mutate.
Idiomatic accumulation rebinds the same name:

```neon
let out: List[i64] = [];
for x in xs {
    out = list::push(out, x * 2);   // O(1) amortised: `out` is uniquely owned
}
```

This is fast because the old `out` is dead the instant it is rebound, so the compiler writes
in place instead of copying. **The one performance cliff**: if you keep the old binding alive
(read it after the write, or alias it), the write must copy to preserve value semantics, and a
loop that does this is O(n²). Rule of thumb: after `acc = push(acc, x)`, never read the
pre-push `acc` again. The compiler has a `stale_thread`/`stale_write` lint that catches the
common cases.

### 3. No `-> null`, and `null` is not "nothing returned".

A procedure that returns no value is written with no arrow (`fn log(s: str) { ... }`) or
`-> ()`. **Never** write `-> null` or `return null` to mean "done". `null` is only ever one
arm of an optional `T | null` — an *absent value*, not a unit return.

```neon
fn find(xs: List[i64], v: i64) -> i64 | null { ... }   // genuine optional
fn greet(name: str) { io::println("hi #{name}"); }     // unit: no arrow
```

### 4. Errors: `throws E` on the signature, and `try` at every call.

Neon has no `Result`/`Option` type. Expected, recoverable failures are declared with
`throws E` **before** the `->`, and optional values are `T | null`. A call to a throwing
function **must** be wrapped in a `try` form — a bare throwing call is a compile error.

```neon
fn parse(s: str) throws string::ParseError -> i64 { ... }

let a = try parse(s);                 // propagate: this fn must also `throws`
let b = try! parse(s);                // trap (panic) on error — only when it cannot fail
let c = try? parse(s);                // soften to `i64 | null`
let d = try? parse(s) orelse 0;       // ...and consume the null with a default
let e = try parse(s) catch (err) {    // handle here; the catch body is the value
    io::println("bad: #{err.message}");
    -1
};
```

Define an error as a record implementing the `Error` protocol (or derive it):

```neon
@derive(Error("cannot open {path}: {reason}"))
record OpenError { path: str, reason: str }
```

### 5. `for x in xs` iterates **lists only**.

There is no general iterator protocol. `for` walks a `List` (and a range `0..n`, which is a
list). For a `Map` or `Set`, iterate `map::keys(m)` / `set::to_list(s)`. Transformation
(`list::map`/`filter`/`fold`) is **eager** — each returns a new list.

```neon
for i in 0..10 { ... }                       // range
for k in map::keys(m) { let v = m[k]; ... }  // maps go through keys
let evens = list::filter(xs, (x: i64) => x % 2 == 0);
```

### 6. No function overloading.

One name is one function. Two behaviours are two names (`max` and `max_by`, `sort` and
`sort_by`), never one name with two signatures.

### 7. Comparison is structural, and unions are not `Ord`.

`T: Ord` is derived from a type's structure — you never implement it. A record is ordered
when all its fields are; an atom, a closure, a `Map`, and a **union** are not. To order by
something other than structure, pass a comparator: `list::sort_by(xs, cmp)`, `cmp::max_by`.

### 8. Generics and turbofish.

`fn f[T](x: T) -> T`, with bounds `where T: Display`. When the type can't be inferred from the
arguments (e.g. it appears only in the return), name it with a turbofish:

```neon
let cfg = try json::decode[Config](text);   // T is in the return, so name it
```

One known limitation: a lambda argument whose type appears only in the lambda's **return**
needs its parameter annotated — `apply(xs, (n: i64) => n + 1)`, not `(n) => n + 1`.

### 9. Protocols are the trait system; derive the boilerplate.

```neon
protocol Display for T { fn to_string(v: T) -> str }
impl Display for Point { fn to_string(p: Point) -> str { "(#{p.x}, #{p.y})" } }

@derive(Display)                    // or write the impl by hand
@derive(json::ToJson, json::FromJson)
record User { name: str, age: i64 }
```

### 10. Concurrency: fibers, and `move` to hand a resource across.

Fibers are stackful green threads; a blocking call parks the fiber, not the OS thread, so you
write straight-line blocking code and it scales. Run a tree with `fiber::runtime`, spawn with
`fiber::spawn`, and talk over channels.

```neon
fiber::runtime(() => {
    let ch = channel::new[i64]();       // turbofish: nothing else pins the element type
    fiber::spawn(() => {                // a plain lambda's captures cross by deep copy
        try! channel::send(ch, compute());
    });
    let result = channel::recv(ch) orelse 0;   // recv is `T | null` (null when closed)
    io::println("#{result}");
});
```

Two things above trip people up: `channel::new` needs a turbofish because no argument reveals
its element type (gotcha #8), and `channel::recv` returns `T | null`, so handle the null.

A `move` lambda (`move () => ...`) is specifically for handing an owned **resource** — a file,
a socket, anything from `std::resource` — into a spawned fiber or a channel, moving it rather
than copying. Using `move` when nothing captured is a resource is an error (`move` moves
nothing here), so a closure that only captures a channel or plain values is a normal `() =>`.
`task::spawn`/`await` give a fiber that returns a value; a crash in an awaited task propagates,
`task::recover` supervises it instead.

## Working in a project

```
neon build        # build the project in ./target
neon run          # build and run
neon check        # type-check only, no output
neon test         # run test blocks
neon fmt          # format
neon doc std::io  # read a stdlib module's docs (or `neon doc` for the list)
```

`neon doc` is the authoritative reference for the standard library — every function carries a
doc-string. When unsure what a stdlib function does or is called, run `neon doc <module>`
rather than guessing.

## A checklist before you call it done

- No `mut`, no `-> null`, no `return null`.
- Every throwing call is inside a `try` / `try!` / `try?` / `catch`.
- Accumulation loops rebind the same name and don't re-read the old binding.
- `match` on a union covers every arm (no `_` needed when the union is closed).
- `neon check` passes.
