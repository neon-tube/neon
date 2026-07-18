# Annotations

An annotation is `@name` or `@name("arg")` written before a `record`, `protocol`,
`impl`, `fn` or `mod`. Each name is handled by exactly one **built-in processor** that
runs in a pass between parsing and type-checking (`compiler/src/expand.rs`), sees the
declaration its annotation is on, and may drop it or pull metadata off it.

## The rules that are settled

- **Built-in only.** The registry is a fixed internal set. There is no user-defined
  compile-time macro — that is a separate, much larger feature (arbitrary code at compile
  time, hygiene, a sandbox) and is deliberately not v1. The processor trait is the same
  shape a user system would need later, so this does not close that door.
- **Five targets:** `record`, `protocol`, `impl`, `fn`, `mod`. An annotation on anything
  else (`type`, `use`, `const`, `test`) does not parse. A method inside a protocol or
  impl is a `fn`, so it is a target too — that is where `@native` lives.
- **Unknown is an error.** A name with no processor is a hard error, not a silent no-op,
  so a typo'd `@cfg` cannot quietly miscompile.
- **The arg is an opaque string; a processor brings its own parser.** `@cfg("all(linux,
  x86)")` parses the string itself. The annotation grammar stays `@name("...")`; the
  meaning of the `...` is the processor's business.

## The pass

`expand(module, config) -> (module, meta, errors)`. It walks the declarations — into a
`mod`'s children and a protocol's or impl's methods — and for each node runs every
annotation's processor. A processor returns *keep* or *omit*; **omit wins**, so any
annotation can drop the node, and a dropped node is gone before the checker sees it (its
unresolved references never error). Metadata (today, `@doc` text) collects in a side
table the driver can use; errors render like any other diagnostic.

Annotations are **left on the AST** after the pass — `@native` is a marker codegen reads
later, and the rest are harmless to the checker.

## The built-in processors

- **`@native("symbol")`** — the fn's body is a runtime symbol. Requires the symbol and a
  body-less fn; only valid on a `fn`. A marker: it never changes the AST.
- **`@doc("text")`** — pulls the text into the metadata table, keyed by the thing it
  documents, and keeps the node. Any target.
- **`@cfg("cond")`** — keeps the node iff `cond` holds against the active config, else
  omits it. `cond` is `key | not(cond) | all(cond, ..) | any(cond, ..)`, evaluated
  against a set of keys the driver seeds from the target (host OS and arch today, until
  cross-compilation exists) and, later, `neon.toml`. A key is true iff it is in the set;
  a malformed condition is an error and, conservatively, keeps the node.

## Not yet

- **Effect tags** (`@native("...") @pure`) — the IR's effect analysis will read a `@pure`
  marker off natives; the processor for it lands with that analysis.
- **Expanding the stdlib.** The pass runs on the program today; running it over the stdlib
  too is what will let `@cfg` select platform-specific stdlib code. Small addition.
- **Node replacement / injection.** The pass supports keep and omit; a processor that
  *rewrites* a node or *adds* declarations (a `derive`) is the natural next capability and
  fits the same walk.
