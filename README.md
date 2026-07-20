# Neon

A small statically-typed language that compiles to C. Values are immutable; the compiler
proves where a mutation is unobservable and does it in place, so the immutable model does
not cost what it usually costs.

```neon
use std::io;

record Point { x: i64, y: i64 }

fn main() {
    let p = Point { x: 3, y: 4 };
    io::println(to_string(p.x + p.y));
}
```

## Building

Needs a Rust toolchain and a C compiler (`cc`, or set `$CC`).

```sh
cargo build --release        # builds the compiler, CLI, LSP, and runtime
```

The CLI is `target/release/neon`. Run `neon doctor` to check the toolchain is wired up.

## Using it

```sh
neon init            # create a project: neon.toml and src/main.neon
neon run             # build and run it
neon build           # build only, into target/
neon compile x.neon  # compile a single file to an executable
neon fmt x.neon      # format
neon check x.neon    # type-check, no output on success
```

`neon ir` prints the intermediate representation, and `neon --help` lists the rest.

## Layout

| | |
|---|---|
| `compiler/` | lexer, parser, type checker, IR, and the C backend |
| `cli/`      | the `neon` command |
| `lsp/`      | language server |
| `runtime/`  | the C runtime the generated code links against |
| `stdlib/`   | the standard library, written in Neon |
| `tests/lang/` | the language test corpus — one `.neon` file per feature, checked against its `.stdout` |
| `verify/`   | Kani proofs of the parts worth a model checker |
| `bench/`    | benchmarks against C and other languages |
| `docs/`     | design decisions and rationale |
| `extra/`    | editor support (tree-sitter, VS Code, Zed, Neovim) |

## Testing

```sh
cargo nextest run        # the whole suite, including the language corpus
verify/run.sh            # the Kani proofs
```
