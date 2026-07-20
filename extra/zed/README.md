# Neon for Zed

Registers `.neon` files as a language and starts [`neon-lsp`](../../lsp) against them.

**There is no syntax highlighting.** That is not an oversight, and as of now it is a
packaging problem rather than a missing grammar; see
[Why there is no highlighting](#why-there-is-no-highlighting) below. Everything else the
server offers does work, because Zed reads capabilities from the `initialize` response
and nothing in this extension has to declare them.

| Feature | Source |
| --- | --- |
| `.neon` recognised as the "Neon" language | `languages/neon/config.toml` |
| Inline diagnostics | `neon-lsp` (`publishDiagnostics`) |
| Format buffer / format on save | `textDocument/formatting` |
| Hover — type or signature, plus the `///` doc comment | `hoverProvider` |
| Go to definition, including into stdlib files | `definitionProvider` |
| Find all references (shadowing-correct) | `referencesProvider` |
| Rename symbol | `renameProvider` |
| Completion (locals and visible fns, with signatures and docs) | `completionProvider`, trigger `:` |
| Signature help | `signatureHelpProvider`, triggers `(` and `,` |
| Outline / symbol search, nested under `mod` and `impl` | `documentSymbolProvider` |
| Inlay hints — types on un-annotated `let`s | `inlayHintProvider` |
| `//` and `///` comment toggling and continuation | `languages/neon/config.toml` |
| `/* */` block comments, bracket matching, 4-space indent | `languages/neon/config.toml` |
| Structural selection, syntax folding, semantic highlighting | **absent** — needs the grammar |

The authoritative capability list is the `ServerCapabilities` literal near the top of
`lsp/src/main.rs`. It is not duplicated in `extension.toml`, deliberately: the copy that
used to live there said "diagnostics and formatting" and stayed wrong through eight
capabilities being added.

Rename declines a symbol whose definition lives in another file and returns an LSP error
rather than a partial edit. That is intended — a rename that silently missed the
definition would be worse than one that refused.

Inlay hints are controlled by Zed's own `inlay_hints` setting, per language:

```json
{
  "languages": {
    "Neon": { "inlay_hints": { "enabled": true } }
  }
}
```

## Install as a dev extension

The extension is a Rust crate compiled to WebAssembly. `extra/` is excluded from the root
Cargo workspace (see the root `Cargo.toml`) precisely so this crate is not built for the
host target as a workspace member.

1. Install the wasm target once:

   ```sh
   rustup target add wasm32-wasip2
   ```

2. Build `neon-lsp` and put it somewhere on your `$PATH`:

   ```sh
   cargo build --release -p neon-lsp
   # then, e.g.
   ln -sf "$PWD/target/release/neon-lsp" ~/.local/bin/neon-lsp
   ```

3. In Zed, open the command palette and run **`zed: install dev extension`**, then select
   this directory (`extra/zed`). Zed compiles the crate itself; you do not need to run
   `cargo build` here.

After editing anything in this directory, run **`zed: reload extensions`**.

## Pointing it at the toolchain

Two things must be found: the server binary, and the sysroot.

### The binary

Resolved in this order:

1. `lsp.neon-lsp.binary.path` in your Zed settings.
2. `neon-lsp` on `$PATH`.

If neither resolves, the extension fails with a message naming both options rather than
launching something that does not exist.

### `NEON_SYSROOT` — read this one

`neon-lsp` loads the standard library from `$NEON_SYSROOT/stdlib`. If that variable is
unset or wrong, **the server still starts and still reports diagnostics** — but
`load_stdlib` returns nothing, the type checker is skipped entirely, and you silently get
lexer and parser errors only. Nothing in the editor indicates this. If Neon files show
syntax errors but never type errors, this is why.

The extension resolves it in this order:

1. `lsp.neon-lsp.binary.env.NEON_SYSROOT` in your Zed settings.
2. `NEON_SYSROOT` inherited from your shell environment.
3. Auto-detected: if the worktree root contains `stdlib/prelude.neon`, the worktree root is
   used. Opening this repository therefore works with no configuration at all.

If none apply, the variable is left unset rather than guessed at.

To set it explicitly, in Zed's `settings.json`:

```json
{
  "lsp": {
    "neon-lsp": {
      "binary": {
        "path": "/absolute/path/to/neon-lsp",
        "env": { "NEON_SYSROOT": "/absolute/path/to/neon/checkout" }
      }
    }
  }
}
```

To format on save, add:

```json
{
  "languages": {
    "Neon": { "format_on_save": "on" }
  }
}
```

A file that does not parse is left untouched by the formatter — it reprints from the AST,
so a half-written line produces no edits rather than an error popup.

## Why there is no highlighting

Zed highlights via tree-sitter, and a grammar must be fetched from a **git repository plus
revision**:

```toml
[grammars.neon]
repository = "https://github.com/..."
rev = "..."
```

`GrammarManifestEntry` in Zed's source makes `repository` and `rev` required and offers no
local-path option, so a grammar cannot be vendored into this directory and used.

**The grammar is no longer the missing piece.** A complete one lives in this repository at
[`extra/tree-sitter-neon`](../tree-sitter-neon): written against `compiler/src/lexer/token.rs`,
`compiler/src/parser/mod.rs` and `compiler/src/ops.rs` rather than against the docs, parsing
308 of the 310 `.neon` files in `tests/lang/` and `stdlib/` cleanly — the two exceptions are
`//@ compile-fail` fixtures that are supposed to be malformed — with `highlights.scm`,
`indents.scm`, `locals.scm` and `textobjects.scm` alongside it.

What is missing is a **fetchable grammar root**. The repository has a remote —
`https://github.com/jkbbwr/neon` — so a `repository` URL now exists. But Zed fetches a
grammar from the *root* of that repository, and this grammar lives at the subpath
`extra/tree-sitter-neon`, so `jkbbwr/neon` on its own does not resolve to a grammar. That is
the remaining blocker: a packaging decision (a dedicated grammar repo, or a Zed that accepts
a subpath), not a missing URL.

Zed's `LanguageConfig.grammar` field is `Option<Arc<str>>`, so omitting it is supported
rather than a hack: the language registers, the file type is recognised, and the language
server attaches. Only tree-sitter-driven features (highlighting, structural selection,
code folding by syntax) are absent.

### Fixing it, once the grammar has a fetchable root

Three edits, all of them small:

1. Add the block to `extension.toml`, pinning a **commit sha** — Zed resolves by revision,
   not by branch. The `repository` is `https://github.com/jkbbwr/neon` if a newer Zed
   accepts a grammar subpath, otherwise a dedicated grammar repository whose root is the
   grammar:

   ```toml
   [grammars.neon]
   repository = "https://github.com/jkbbwr/neon"
   rev = "…"
   ```

2. Set `grammar = "neon"` in `languages/neon/config.toml`.
3. Copy `extra/tree-sitter-neon/queries/highlights.scm` (and `indents.scm`) into
   `languages/neon/` — **and widen the capture names while copying.** The queries use
   Neovim's fine-grained vocabulary (`@keyword.conditional`, `@variable.member`,
   `@type.definition`, `@function.method`, `@number.float`, `@module`, `@error`). Zed maps
   a capture it does not recognise to *nothing* rather than to a fallback, so a verbatim
   copy leaves large regions unstyled. Add coarse duplicates (`@keyword`, `@property`,
   `@type`, `@number`) beside the fine ones. This is written up under "Capture-name
   divergence" in the grammar's README.

Note that the grammar's external scanner is mandatory, not optional: Neon's block comments
nest, no regular expression can count, and the depth is tracked in `src/scanner.c`. Zed
compiles `scanner.c` automatically when it is present in `src/`, which it is.

### A warning about older grammars

`github.com/jkbbwr/neon` now hosts *this* repository, whose grammar is the current one at
`extra/tree-sitter-neon`. But an older Neon grammar circulated before this — with
`enum_declaration`, `if_let_expr`, `map_init` and `type_nullable`, none of which exist now,
no rule for string interpolation, `marker`, `bench` or `assert_throws`, and entirely
different node names (`binary_expr` against `binary_expression`, `int_literal` against
`integer`, and so on). Do not point Zed at any such copy. Queries written for the current
grammar do not merely degrade against it, they fail to compile — observed in practice, in
Neovim: an installed copy of the old parser makes the new `highlights.scm` error out with
`Invalid node type "doc_comment"`. If you pin a `rev`, pin one from the current history.

## What has and has not been verified

Verified on this machine:

- `cargo build --release --target wasm32-wasip2` succeeds and produces a wasm component;
  `cargo clippy` is clean. Resolved `zed_extension_api` 0.7.0, the current published
  release and the same version other installed extensions use.
- Both TOML files parse, and every key used was checked field-by-field against Zed's
  `ExtensionManifest`, `GrammarManifestEntry` and `LanguageConfig` structs in Zed's source
  rather than against the documentation. (The docs claim `grammar` is required in a
  language config; the source says otherwise, and the source is what runs.)

Not verified:

- **The extension has not been loaded into a running Zed.** No `zed` binary was reachable
  from this shell, so "installs and attaches successfully" is reasoned from Zed's source,
  not observed. Confirm with `zed: install dev extension` and check the language server
  logs (`zed: open log`).
- **The feature table above is a capability list, not an observation.** Every row was read
  off the `ServerCapabilities` literal in `lsp/src/main.rs`, and the ten capabilities were
  confirmed to arrive in an `initialize` response — but by a Neovim client, not by Zed.
  Zed consuming each of them is expected rather than seen.
- **Whether a newer Zed offers a local-path grammar option.** The claim that
  `GrammarManifestEntry` requires `repository` + `rev` is inherited from the earlier
  check against Zed's source and could not be re-checked here: no `zed` binary, no network,
  and `zed_extension_api` 0.7.0 carries nothing about grammars (they are resolved entirely
  on Zed's host side, not through the guest API). If you can reach Zed's source, re-check
  it before trusting that paragraph — it is the one claim here with no live evidence behind
  it.
