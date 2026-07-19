# Fuzzing the front end

Three libFuzzer targets over the lexer, the parser and the formatter. The first
two assert that nothing panics; the third asserts the formatter's actual
contract, and is the one worth running for hours.

`fuzz/` is a **detached workspace** — `fuzz/Cargo.toml` carries an empty
`[workspace]` table, and the root `Cargo.toml` lists `fuzz` under `exclude`.
`cargo build` and `cargo nextest run` at the repo root do not see it and do not
acquire a nightly dependency.

## Requirements

cargo-fuzz needs nightly. The default toolchain here is stable, so every command
below is spelled `cargo +nightly`.

```sh
cargo install cargo-fuzz          # 0.13.2 or later
rustup toolchain install nightly
```

## Running a target

`lex` is simple:

```sh
cargo +nightly fuzz run lex -- -dict=fuzz/neon.dict -max_total_time=60 -only_ascii=1
```

`parse` and `format` need three extra things, because `parser::parse` leaks (see
**Known issues**). Run them through the built binary so the ASan options reach
the fork children:

```sh
cargo +nightly fuzz build parse

ASAN_OPTIONS=detect_odr_violation=0:detect_leaks=0 \
  fuzz/target/x86_64-unknown-linux-gnu/release/parse \
  fuzz/corpus/parse -artifact_prefix=fuzz/artifacts/parse/ \
  -dict=fuzz/neon.dict -max_total_time=300 -max_len=16384 \
  -only_ascii=1 -fork=1
```

- `detect_leaks=0` — every `parse()` call leaks, so LeakSanitizer fires on the
  first unit and nothing else ever runs. It must go in `ASAN_OPTIONS` rather
  than as a `-detect_leaks=0` flag: in fork mode the flag is not passed to the
  children, and they will report the leak anyway.
- `-fork=1` — turning the *report* off does not stop the leak, so RSS climbs
  monotonically and the run dies of OOM at the 2 GB default after a couple of
  minutes. Fork mode restarts the child periodically and resets it.
- `-only_ascii=1` — the lexer has an open panic on a non-ASCII escape (see
  **Known crashes**) that both targets hit within seconds, masking everything
  downstream. Drop this once that is fixed.

Other useful flags: `-max_len=16384` (the largest seed is ~14 KB, so a bigger cap
only buys slower runs), `-jobs=N -workers=N` to fan out, `-print_final_stats=1`
for the iteration count.

## What each target asserts

| Target | Assertion |
| --- | --- |
| `lex` | `lex_full` does not panic, and every token and trivia span is an in-bounds, non-reversed, char-boundary-aligned slice of the source. The formatter reprints literals by slicing with a token's span, so a dishonest span is a formatter bug in waiting. |
| `parse` | `parse` does not unwind on any token stream; it always returns a module or an error (never neither); every error span is sliceable; and the resulting tree survives a full `strip_spans` walk. |
| `format` | The formatter's contract, for any input that lexes and parses cleanly: **round-trip** (`parse(format(src)) == parse(src)` after `strip_spans`), **idempotence** (`format(format(src)) == format(src)`), and **comment preservation**. |

`format` is the valuable one. It is `compiler/tests/corpus_roundtrip.rs` with the
corpus swapped for a fuzzer: the corpus pins the contract over shapes someone
thought to write, this pins it over the shapes nobody did. The formatter has
silently changed meaning before — it reprinted `1 - (2 - 3)` as `1 - 2 - 3`,
because it emitted a binary operator's right operand at the parent's precedence
instead of its own. A corpus only catches that if someone happened to write a
right-nested subtraction. A fuzzer that reports "formatting changed the tree" has
found a miscompile.

## Inputs are UTF-8

All three targets do `str::from_utf8(data)` and return early if it fails, rather
than reaching the byte level. `lex` takes `&str`, so non-UTF-8 is not a reachable
state: no caller could produce one, and fuzzing bytes would mean inventing a
lossy decode and then reporting crashes on inputs the function cannot be given.
The cost is that some mutations are discarded; the dictionary and the real-program
seed corpus keep that fraction small, and it buys a corpus of literal `.neon`
text that a human can read and `neon fmt` can be pointed straight at.

Multi-byte input is still very much in scope — Neon string and rune literals hold
arbitrary text, the seeds contain non-ASCII, and the lexer indexes a `&[u8]`
while slicing a `&str`. That is exactly the shape that panics on a char boundary,
and it is exactly what the first run found.

## Seeds

`fuzz/seed.sh` copies every `tests/lang/**/*.neon` and `stdlib/**/*.neon` (281
files) into `fuzz/corpus/{lex,parse,format}/`, named by content hash so re-running
is idempotent.

```sh
fuzz/seed.sh
```

Hundreds of real programs is a far better starting point than random bytes:
`tests/lang` is the language specification and `stdlib` is the largest body of
Neon that exists, so between them they reach grammar libFuzzer would take a very
long time to stumble into.

## Dictionary

`fuzz/neon.dict` is **generated** — do not edit it.

```sh
fuzz/gen-dict.py
```

It derives the keywords from `Token::keyword` and the operators from `Token`'s
`Display` impl in `compiler/src/lexer/token.rs`. Both tables are exhaustive by
construction (`Display` has no catch-all arm, so a new token cannot be added
without giving it a name), which is why the dictionary is derived rather than
transcribed: a hand-written one goes stale the first time someone adds a keyword.
Regenerate it when the token alphabet changes.

## Reproducing a crash

libFuzzer writes the failing input to `fuzz/artifacts/<target>/crash-<hash>`.

```sh
# Re-run the one input.
cargo +nightly fuzz run lex fuzz/artifacts/lex/crash-<hash>

# Shrink it. Usually gets to a handful of bytes.
cargo +nightly fuzz tmin lex fuzz/artifacts/lex/crash-<hash> -- -max_total_time=120

# Look at it as text, since these are (valid UTF-8) Neon source.
cat fuzz/artifacts/lex/crash-<hash>; xxd fuzz/artifacts/lex/crash-<hash>
```

A minimised artifact worth keeping goes in `fuzz/known-crashes/`, which is
tracked, with a note below. Everything in `fuzz/artifacts/` is transient.

The built target is also a plain binary that takes files as arguments, which is
the quickest way to test a hand-written repro:

```sh
cargo +nightly fuzz build lex
fuzz/target/x86_64-unknown-linux-gnu/release/lex my-repro.neon
```

## Known crashes

### `lex-escape-char-boundary` — a non-ASCII escape panics the lexer

```
compiler/src/lexer/mod.rs:537
start byte index 3 is not a char boundary; it is inside 'ޘ' (bytes 2..4 of string)
```

Repro (4 bytes, `fuzz/known-crashes/lex-escape-char-boundary`): a `"`, a `\`, and
any multi-byte character. In readable form:

```neon
let s = "a\éb";
```

`Lexer::escape` reads the escaped character with `bump()`, which advances `pos`
by **one byte**. For an unrecognised escape whose character is multi-byte, that
leaves `pos` in the middle of a UTF-8 sequence; the string-body loop's next
iteration does `self.text[self.pos..]` at `mod.rs:537` and panics. Reported as a
lex error first, so the lexer is already on the "keep going after a bad escape"
path — it just resumes at a bad offset.

The rune path does not reproduce: `'\é'` lexes without panicking, so the fix is
in the string body's resumption, not in `escape`'s error reporting.

## Known issues

### `parser::parse` leaks its parser on every call

Not a fuzzer finding — it reproduces on any input, including the corpus — but the
fuzzer is what made it visible, and it is the reason `parse` and `format` need
`detect_leaks=0` and `-fork=1`.

LeakSanitizer reports roughly 17 KB per `parse()` call (~68 KB for `format()`,
which parses twice). The allocations are `Rc<RcInner<chumsky::combinator::...>>`
under `chumsky::recursive::Recursive`, reached from `parser::binary_ops`
(`parser/mod.rs:1521`) and `parser::module` (`parser/mod.rs:131`). This is
chumsky's `recursive()` building an `Rc` cycle: the recursive parser holds a
handle to itself, so its refcount never reaches zero and the whole parser graph
is freed only by process exit. The parser is rebuilt on every `parse()` call, so
the leak is per-call rather than one-time — 41,540 fuzz iterations reached 2 GB
RSS and were killed.

For the batch compiler this is invisible: one parse, then exit. For the **LSP**,
which reparses on every keystroke, it is unbounded growth in a long-lived
process, and that is where it is worth fixing. The usual remedy is to build the
parser once into a `OnceLock`/`Cached` and reuse it, which also removes the
per-call construction cost.

## Gitignore policy

`fuzz/corpus/` and `fuzz/artifacts/` are both ignored, along with `fuzz/target/`
and `fuzz/coverage/`. `fuzz/known-crashes/` is tracked.

- **corpus** — libFuzzer writes thousands of files there and rewrites them every
  run. Committing it means a churning binary blob in every diff, for no benefit:
  the seeds that matter (`tests/lang`, `stdlib`) are already tracked, and
  `seed.sh` rebuilds the directory in about a second.
- **artifacts** — crash inputs from the last run, transient by definition. A
  crash worth keeping has been minimised and moved to `known-crashes/`, where it
  is a few bytes with an explanation next to it rather than an unlabelled hash.
