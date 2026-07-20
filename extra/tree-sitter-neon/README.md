# tree-sitter-neon

A [tree-sitter](https://tree-sitter.github.io) grammar for Neon.

The grammar is derived from the compiler, not from the documentation:

| Source | What it settles |
| --- | --- |
| `compiler/src/lexer/token.rs` | the token alphabet and the reserved-word list |
| `compiler/src/lexer/mod.rs` | literal forms, comments, string interpolation |
| `compiler/src/parser/mod.rs` | the grammar itself, in chumsky |
| `compiler/src/ops.rs` | the one binary-operator precedence table |

`ops.rs` matters most. It is the single ladder that both the parser and the
formatter read — a second copy is what once made the formatter reprint
`1 - (2 - 3)` as `1 - 2 - 3`. The `PREC` table at the top of `grammar.js` is
that table transcribed, level for level. **If `ops.rs` changes, change it here
too**, and re-run the precedence tests in `test/corpus/expressions.txt`.

## Status

Parses **308 of the 310** `.neon` files in `tests/lang/` and `stdlib/` (299 and
11 respectively) with no `ERROR` or `MISSING` node. Both exceptions are
`//@ compile-fail` fixtures whose
whole purpose is to be malformed, so an error node is the correct answer for
each:

- `tests/lang/strings/interpolation_unterminated_fails.neon` — a deliberately
  unterminated interpolation.
- `tests/lang/strings/unknown_escape_of_a_multibyte_char.neon` — an escape that
  is not in the lexer's set.

`tree-sitter generate` is clean: no unresolved conflicts, and no unnecessary
ones. The **nine** declared conflicts are each a genuine local ambiguity that
the compiler also has to resolve, and each is commented in `grammar.js`.

`tree-sitter test` passes all **27** cases in `test/corpus/`.

## Building

```sh
npm install          # only needed for the tree-sitter CLI
tree-sitter generate # regenerate src/ from grammar.js
tree-sitter test     # run test/corpus
tree-sitter build    # build a shared object for local use
```

`src/` (`parser.c`, `scanner.c`, `grammar.json`, `node-types.json`) is
committed, as is conventional, so a consumer needs no build step and no
tree-sitter CLI.

To re-check the corpus:

```sh
for f in $(find ../../tests/lang ../../stdlib -name "*.neon"); do
  tree-sitter parse -q "$f" >/dev/null || echo "FAIL $f"
done
```

### The external scanner

`src/scanner.c` exists for exactly one reason: Neon's block comments **nest**,
so a commented-out block containing a comment does not end early. No regular
expression can count, so the nesting depth is tracked in C. Any consumer must
compile `scanner.c` alongside `parser.c`.

## Editor setup

Everything below assumes the grammar is fetched from this repository at
`extra/tree-sitter-neon`, and that `scanner.c` is compiled in.

### Neovim

**Use [`extra/neovim`](../neovim), which does all of this already.** Its
`setup{}` registers the parser and starts it, `queries/neon/*.scm` there are
symlinks into this directory so the two cannot drift, and `syntax/neon.vim`
covers the buffer until `:TSInstall neon` has been run.

By hand, if you would rather not take the plugin:

```lua
require('nvim-treesitter.parsers').get_parser_configs().neon = {
  install_info = {
    url = 'https://github.com/jkbbwr/neon',
    location = 'extra/tree-sitter-neon',
    files = { 'src/parser.c', 'src/scanner.c' },
  },
  filetype = 'neon',
}
vim.filetype.add({ extension = { neon = 'neon' } })
```

`files` must list `scanner.c`. A parser built from `parser.c` alone links and
loads and then gets every nested block comment wrong — see below.

Then copy `queries/` into `~/.config/nvim/queries/neon/`.

One thing to check first: if a `neon` parser is **already** installed, it is
probably the predecessor repository's, whose node names are different. The
queries here do not degrade against it, they fail to compile —
`Invalid node type "doc_comment"`. `:TSUninstall neon` before `:TSInstall neon`.

### Zed

Not wired up yet, and not for want of a grammar: Zed fetches a grammar by git
`repository` + `rev`, and this repository has no remote. The full reasoning, and
the three edits that resolve it, are in [`extra/zed/README.md`](../zed/README.md).
Note that Zed reads a narrower set of capture names than Neovim, so the copy
under `languages/neon/` needs coarse duplicates added; see the divergence note
below.

### Anything using the `tree-sitter` CLI

`tree-sitter.json` declares the scope, file types and highlight query, so
`tree-sitter highlight some.neon` works once this directory is on your
`parser-directories`.

## Queries

| File | Verified how |
| --- | --- |
| `queries/highlights.scm` | `tree-sitter highlight --check` over all 310 corpus files, zero failures |
| `queries/locals.scm` | compiles and captures correctly under `tree-sitter query` |
| `queries/indents.scm` | compiles and captures correctly under `tree-sitter query` |
| `queries/textobjects.scm` | compiles and captures correctly under `tree-sitter query` |

`locals.scm` gives the editor scopes, definitions and references, which is what
drives "highlight every other occurrence of the name under the cursor", local
go-to-definition, and scope-aware rename. It is a purely **lexical**
approximation — nothing in it knows about types, protocol dispatch or module
resolution — so a reference that actually resolves to an import or a global
finds no definition and is left alone. That is the intended failure mode: no
answer is much better than a confidently wrong one when the payload is a rename.

Two things in it are worth knowing. Declarations are scopes in their own right
and not merely via their bodies, because a `fn`'s parameters and type parameters
live outside its `block` and a `where` clause has to be able to see the type
parameter it bounds. And each `match_arm` is its own scope, so an arm's pattern
bindings cannot leak sideways into the next arm.

There is deliberately **no `injections.scm`**. The obvious candidate would be
string interpolation, but `"a #{expr} b"` is not an injection: the lexer emits
the hole as a real token run and the grammar parses `expr` as ordinary Neon, so
`(interpolation)` already contains first-class expression nodes. Injecting a
second parser there would be strictly worse.

Two decisions in `highlights.scm` are worth surfacing here, because both trade
something and the trade is not obvious from the file:

- **`i64`, `f64`, `str` and `bool` are matched by spelling, everywhere.** Only
  `any` is a keyword with a node of its own; the rest are ordinary identifiers,
  so an `#any-of?` predicate is the only tool available. That predicate is not
  restricted to type position — repeating the thirty-odd type patterns with the
  predicate bolted on would be thirty more chances to miss one when a type
  context is added. The price is that a *value* named `str` renders as a builtin
  type. Every binding site re-captures its own name afterwards, so a definition
  spelled `str` still wins; only a use of one is affected. The VS Code TextMate
  grammar makes the same trade.

- **An unrecognised `@annotation` is captured as `@error`.** The name set is
  closed: `expand.rs`'s `lookup()` maps exactly `native`, `cfg`, `doc`,
  `runtime`, `pure` and `inline`, and `run()` reports anything else as "unknown
  annotation", which fails the build. Colouring an unknown one red is not a
  guess — it is the compiler's own answer, delivered sooner. **Keep the list in
  `highlights.scm` in step with `lookup()`.**

  That warning was here and was not sufficient. `@inline` was added to the
  compiler afterwards, the list was not updated, and the five `@inline` uses in
  `stdlib/std/collections/list.neon` were highlighted as errors — which
  `tree-sitter highlight --check` passes happily, because a *wrong* colour is
  still a colour. The same stale five-name list was in the Neovim syntax file,
  the VS Code TextMate grammar and both READMEs. If a seventh annotation lands,
  it has to be changed in all of them; `grep -rl 'runtime.*pure' extra/` finds
  the set.

`indents.scm` and `textobjects.scm` are verified to compile and to capture the
nodes they name. They are *not* verified against Neovim's indent or textobject
behaviour end to end, because that needs a Neovim harness this repository does
not have. Treat them as a good starting point rather than as tested.

`indents.scm` handles continuation lines as well as bracketed bodies — a wrapped
binary expression, a `|>` pipeline broken across lines, a multi-line `where`, a
method chain broken before the dot. Those rely on nvim-treesitter counting at
most one `@indent.begin` per starting *row*, which is what stops a left-nested
pipeline from indenting once per link. One case is deliberately unhandled: a
`->` return type moved onto its own line. The only node spanning both the
signature and that line is `function_declaration`, and capturing it would put
every function *body* two levels in whenever the opening brace starts a row of
its own — a wrong indent on every function, to fix a rare line break.

### Capture-name divergence

The queries target the capture names Neovim and Zed share. Two things differ:

- **Ordering.** Neovim and Zed both let a *later* matching pattern override an
  earlier one, so `highlights.scm` goes general → specific: the broad
  `(identifier) @variable` is near the top and every later pattern narrows it.
  The `tree-sitter highlight` CLI resolves ties the other way, so its output is
  coarser than what an editor shows. That is a CLI limitation, not a query bug.
- **Vocabulary.** Neovim understands the fine-grained names used here
  (`@keyword.conditional`, `@keyword.repeat`, `@keyword.exception`,
  `@variable.member`, `@type.definition`, `@type.builtin`, `@function.method`,
  `@constructor`, `@character.special`, `@number.float`, `@module`, `@error`).
  Zed maps unknown captures to nothing, so on Zed those fall back to unstyled.
  If you care, add coarse duplicates (`@keyword`, `@property`, `@type`,
  `@number`) in the Zed extension's copy of the file.

## Known divergences from the compiler

One place where this grammar deliberately does not match
`compiler/src/parser/mod.rs`, and it does not affect the corpus.

1. **Condition position is handled by GLR, not by a second grammar.** The
   compiler builds a whole second expression grammar (`cond`) with record
   literals switched off, so `while a { }` cannot read `a { }` as an empty
   record. Here both readings are explored and the record reading dies for want
   of a block, which reaches the same answer without doubling every rule. As in
   the compiler, parenthesise to get a record literal back: `while (a { }) { }`.

   This is a superset, not a mismatch: everything the compiler accepts in
   condition position this grammar accepts and shapes identically. The only
   observable difference is on input the compiler rejects, where GLR may find a
   tree the compiler never would.

### Divergences that used to be here and are not any more

- **Turbofish arguments were restricted** to a `_simple_type` subset — types
  that cannot begin like a parenthesised expression. That was worse than the
  ambiguity it avoided. `f[(A, B)](1)` and `f[{a: i64}](1)` still parsed, but
  *silently* as an index of a tuple or record literal that was then called, and
  `f[(i64) -> str]()` produced an `ERROR`. A wrong tree with no error node is
  the worst available outcome, because highlighting and textobjects consume it
  with no signal that it is nonsense. `turbofish_arguments` now takes the full
  type grammar, matching `parser/mod.rs:1507`, and the ambiguity is handed to
  GLR via the `[$._expression, $._type]` conflict.

  `f[T](x)` is *also* a well-formed index of `f` whose result is called, for
  every `T`, so both readings survive to end of input and something has to
  break the tie. The compiler breaks it in `postfix_ops`, where the turbofish is
  an `.or_not()` ahead of the argument list and chumsky tries the `Some` branch
  first. `prec.dynamic(PREC.turbofish)` on `turbofish_arguments` is that same
  preference. Fixing this also corrected `identity[i64](5)` in
  `test/corpus/expressions.txt`, which had been pinning the wrong tree.

- **`{}` was a block, not an empty record literal.** The compiler's `atom_expr`
  tries `record_lit` (`parser/mod.rs:1243`, whose path *and* field list are both
  optional) before `block_expr` (`:1318`), so a bare `{}` in expression position
  is an empty record. This is a true ambiguity rather than a lookahead problem —
  the two tokens are the same forever — so GLR cannot settle it either, and it
  is settled the same way the compiler settles it: by ordering.
  `prec.dynamic(PREC.empty_record)` on the `{}` alternative of `record_literal`
  picks the record. It is dynamic and not static precedence because a static
  `prec` sits on the whole rule and would tilt every other block-vs-record
  decision with it, starting with `while a { }`, which must keep reading `{ }`
  as the loop body.

  A brace that is required by position — a function body, an `if` consequence, a
  loop body — is a `block` field in the grammar and never a candidate for the
  record reading, so none of that moved.

Both are pinned by `test/corpus/ambiguities.txt`.

## Relationship to the older grammar

The predecessor repository (`jkbbwr/neon`, `extra/tree-sitter-neon` at
`fc53a03`) also has a Neon grammar. This one is **written fresh, not derived
from it**, and the node names are **not compatible**.

That grammar describes an older language: it has `enum_declaration`,
`enum_pattern`, `if_let_expr`, `let_expr`, `map_init`, `list_pattern`, `sigil`
and `type_nullable`, none of which exist in Neon now, and it has no rule for
string interpolation, `marker`, `bench` or `assert_throws`. Its precedence
ladder also disagrees with `ops.rs` — it has a `cast` level and no `orelse`
level at all. Adapting it would have meant rewriting every rule anyway, without
the benefit of having checked each one against the current parser.

Its naming convention differs throughout (`binary_expr`, `call_expr`,
`int_literal`, `string_literal`, `type_spec`, `module_path` against this
grammar's `binary_expression`, `call_expression`, `integer`, `string`, `_type`,
`path`). **Any editor configuration written against the old grammar — including
an installed Zed extension — needs its queries updated, not just repointed.**
