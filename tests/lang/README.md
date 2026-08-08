# The language corpus

These files **are** the language specification. There is no prose spec to drift out of
sync — if you want to know what Neon does, read these; if you want to change what Neon
does, change these first.

Each file is a complete Neon program plus a golden. The harness compiles it, links it,
runs the binary, and diffs stdout and the exit code.

## Layout

    tests/lang/<area>/<name>.neon      the program
    tests/lang/<area>/<name>.stdout    exact expected stdout (required unless compile-fail)
    tests/lang/expected-pass.txt       the ratchet — see below

One behaviour per file. Small, focused, deterministic. A failure should name the
behaviour, not send you spelunking.

## Directives

`//@` lines in the **leading comment block** — the block ending at the first line that is
neither blank nor a `//` comment. Anything later is ignored.

    //@ exit: <n>                expected exit code of the binary. Default 0.
    //@ args: <a> <b> ...        command-line arguments for the run, whitespace-separated
                                 (no quoting; an argument with a space has no spelling
                                 here). They appear from `os::args()[1]` on — index 0 is
                                 the program path, which varies and must not be asserted.
    //@ env: KEY=value           set one environment variable for the run. Repeatable.
                                 `KEY=` sets the empty string, which `os::env` keeps
                                 distinct from unset. The parent environment is inherited
                                 too, so test names should be obscure enough not to
                                 collide with it. stdin is always null: `io::read_line()`
                                 answers `null` immediately.
    //@ compile-fail             compilation must fail. No .stdout needed; binary never run.
    //@ error-contains: <substr> checked against the diagnostic MESSAGES. Repeatable; all
                                 must match. ANSI codes are stripped first. Only with
                                 compile-fail.

                                 Messages, NOT the rendered output. The render carries the
                                 file path and echoes the offending source line, so any
                                 substring occurring in either matches whatever the
                                 compiler actually said. `error-contains: main` in
                                 main_throws_clause_is_fixed.neon passed on the strength of
                                 its own filename, while the real error was `unknown
                                 protocol Error`. Five files were passing that way.

                                 So: match text that can only come from a message. A
                                 substring that also appears in the program, the path or a
                                 keyword you wrote is asserting nothing.

## The ratchet: expected-pass.txt

Lists the corpus files that **must** pass. It starts empty and grows.

- Listed and failing        -> the build fails. A regression.
- Not listed and failing    -> reported, does not fail. Not built yet, or built wrong.
- Not listed and passing    -> the build fails: "now passing, add it to expected-pass.txt".

That last rule is the point. You cannot silently regress, and you cannot silently forget
to record a win. Progress is one file you can read.

This is the only mechanism. "Not implemented yet" and "implemented wrong" are the same
state — absent from the list — so they need no separate marker.

## Writing a test

Write it as if the language already worked — golden and all. The corpus describes the
language we intend, so a file lands before the feature does. That is the point: the
compiler is implemented against these, not documented by them.
