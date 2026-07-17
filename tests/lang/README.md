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
    //@ compile-fail             compilation must fail. No .stdout needed; binary never run.
    //@ error-contains: <substr> checked against compiler stderr. Repeatable; all must
                                 match. ANSI codes are stripped first, so match the plain
                                 text you would read on screen. Only with compile-fail.

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
