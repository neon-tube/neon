# Annotations

An annotation is `@name` or `@name(arg, ..)` written before a `record`, `protocol`, `impl`,
`fn` or `mod`. Each name is handled by exactly one **built-in processor** that runs in a
pass between parsing and type-checking (`compiler/src/expand.rs`), sees the declaration its
annotation is on, and may drop it or pull metadata off it.

## The rules that are settled

- **Built-in only.** The registry (`expand::lookup`) is a fixed internal set. There is no
  user-defined compile-time macro — that is a separate, much larger feature (arbitrary code
  at compile time, hygiene, a sandbox) and is deliberately not v1. The processor trait is
  the same shape a user system would need later, so this does not close that door.
- **Five targets:** `record`, `protocol`, `impl`, `fn`, `mod`. An annotation on anything
  else (`type`, `use`, `const`, `test`) does not parse. A method inside a protocol or impl
  is a `fn`, so it is a target too — that is where `@native` lives. Every processor that
  restricts its target phrases the rejection through `Target::what`, so a new target kind
  cannot be described inconsistently in five places.
- **Unknown is an error.** A name with no processor is a hard error, not a silent no-op, so
  a typo'd `@cfg` cannot quietly miscompile.
- **An argument is a name; a string is where the language stops.** The grammar is
  `arg := path | path(arg, ..) | "text"`, so `@cfg(all(linux, x86_64))` and
  `@derive(Display)` are parsed once, by the parser, into `ast::AnnArg`. A processor
  validates a *shape* — it does not parse. Quoting is reserved for what is genuinely not a
  Neon name: a C symbol (`@native("neon_str_len")`), a C type (`@runtime("NeonBytes*")`),
  prose (`@doc("...")`). This replaced an earlier rule that the arg was always an opaque
  string; `docs/decisions.md` records what that cost. The one consequence to know: a config
  key must be an identifier, so it cannot contain `-`.
- **A processor cannot rewrite the AST.** Its `Context` carries the config, the metadata
  table and the error list — and not the node — so the only decisions available are *keep*
  and *omit*. That restriction is what keeps expansion from becoming a macro system by
  accident.

## The pass

`expand(module, config) -> (module, meta, errors)`. It walks the declarations — into a
`mod`'s children and a protocol's or impl's methods — and for each node runs every
annotation's processor. **Omit wins**, so any annotation can drop the node, and a dropped
node is gone before the checker sees it (its unresolved references never error). A
`@cfg`-omitted `mod` returns without its body ever being walked, so a `@doc` inside a
dropped branch never reaches the metadata table. Errors render like any other diagnostic.

A fn *body* is never expanded: `@cfg` selects declarations, not statements.

Annotations are **left on the AST** after the pass. That is not incidental — three of the
five processors are pure validators whose *effect* is that a later stage reads the
annotation back off the AST:

- `env.rs::declare` reads `@runtime` off a record to fill `Types::runtime_types`;
- `lower.rs` reads `@pure` (via the checker's fn table) to build `Program::pure_natives`;
- codegen reads `@native` to emit a call to the symbol.

`Meta` — the side table `expand` returns — carries the `@doc` text and a second copy of the
`@runtime` mapping. The driver currently discards it (`cli/src/frontend.rs` binds it to
`_meta`), so the metadata path is built but unused; the readers above go to the AST
directly. Worth knowing before adding a processor that only fills `Meta`.

*Undocumented, recorded rather than explained:* the driver expands the **user's module
only**. The stdlib is parsed by `stdlib::parse_from` and never runs through `expand`.
`@runtime` and `@pure` in stdlib sources still work, because their readers go to the AST
rather than to `Meta` — but a stdlib `@cfg` would be silently ignored and a stdlib
annotation typo is never diagnosed. No reasoning for this split is recorded anywhere in the
code; it looks like an omission rather than a decision.

## The built-in processors

- **`@native("symbol")`** — the fn's body is a runtime symbol. Requires the symbol and a
  body-less fn; only valid on a `fn`. A marker: it never changes the AST.
- **`@pure`** — this native has no effect beyond its return value, so a call whose result is
  unused may be deleted. Takes no argument, and is **only** valid alongside `@native`: a
  Neon body's purity is *inferred* from its instructions, so claiming it by hand would be
  either redundant or a lie the compiler would believe. The absence of `@pure` means
  effectful, and that polarity is the whole design — forgetting it costs an optimisation,
  while a wrong `@pure` licenses DCE to delete a call that mattered. The analysis this
  replaced guessed purity from the symbol's *spelling* and defaulted to pure, which silently
  removed a resource construction along with the cleanup it existed to schedule. See
  `docs/design/ir.md` § Effects and `compiler/src/ir/effects.rs`.
- **`@runtime("neon_file")`** — the record is a pointer to a C type the runtime owns, not a
  struct laid out from its fields. Only valid on a `record`, which must declare **no fields**:
  its contents are the runtime's business, and a field here would claim a layout the C type,
  not the compiler, decides. Generic parameters are fine and are carried through as the
  repr's arguments, which is how a payload's element type reaches the backend so a witness
  can be emitted for it. This is the declaration form of what used to be a name the compiler
  recognised — `List`, `Map` and `File` were matched by string in `record_repr` — so a
  runtime-backed type can now live in an ordinary stdlib module. It produces
  `Repr::Runtime { nominal, c_type, args }`; see `docs/design/ir.md` § Representations for
  why both names are carried.
- **`@doc("text")`** — pulls the text into the metadata table, keyed by the name of the thing
  it documents, and keeps the node. Any target. The key is a bare name, so two same-named
  things in different modules collide; that is tolerable only because `Meta::docs` is a `Vec`
  of pairs nothing looks up by key yet.
- **`@cfg(cond)`** — keeps the node iff `cond` holds against the active config, else omits
  it. `cond` is `key | not(cond) | all(cond, ..) | any(cond, ..)`, evaluated against a set of
  keys the driver seeds from the host OS and arch (until cross-compilation exists) and, later,
  `neon.toml`. A key is true iff it is in the set — an unknown key is *false*, not an error,
  because there is no registry of legal keys and asking about one this build never heard of
  is the normal case. The set is built once and read-only thereafter, so a condition cannot
  flip mid-pass and drop both of two mutually exclusive branches. `expand` no longer parses
  the condition — the grammar does — so an unbalanced one is a parse error; what remains a
  `@cfg` diagnostic is a condition that parses but cannot mean anything (`not` with two
  operands, an empty `all`, a key applied to arguments), and those conservatively keep the
  node.
- **`@derive(Display)`** — the record gets an ordinary generated `impl`, written by
  `compiler/src/derive.rs`. Only on a `record`; several protocols per annotation
  (`@derive(Display, Eq)`) are several arguments, not a list inside one. The processor
  itself only *validates*, because generating a declaration means holding the AST and
  `Context` deliberately does not; the generation is called from `expand` directly, after
  the processor walk, so a `@cfg`-omitted record derives nothing. The protocol path is
  carried into the generated impl unchanged and resolved there like any other — this pass
  resolves nothing, and the derivable set is matched on the last segment.

## Not yet

- **Expanding the stdlib.** See the note above: the pass runs on the user's module only.
  Running it over stdlib sources too is what would make `@cfg` usable for platform-specific
  stdlib code, and would put stdlib annotation typos through the same diagnostics as
  everything else. It now also gates `@derive`: a stdlib `@derive` is silently ignored,
  because the pass that writes the impl never sees stdlib sources.
- **Node replacement.** A processor that *rewrites* the node it sits on still does not
  exist, and `Context` still cannot see the AST. `@derive` shows the shape the line actually
  holds: adding declarations is a pass over the module, called from `expand`, while what a
  *processor* may decide is unchanged — keep or omit, never rewrite.
