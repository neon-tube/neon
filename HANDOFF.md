# Handoff: continuing Neon after the refcount redesign

Read this, then `docs/finalpush.md`, before doing anything. Repo: `~/projects/personal/neon2`
(not `neon` — that's an older sibling checkout; ignore it).

## State

- **698/698 suite, 153/153 backend corpus under ASan+UBSan. Zero leaks, zero UAF, zero UB.**
- Branch `main`. If `git status` shows modified `compiler/src/ir/refcount.rs`,
  `refcount/tests.rs`, `docs/design/ir.md`, `docs/finalpush.md`, `HANDOFF.md`: that is the
  finished, verified refcount redesign — commit it as one commit before starting new work.
  No attribution trailers.
- The refcount pass was **rewritten, not patched**, on 2026-07-19. The release-placement
  spec now exists in two places that must stay in sync with the code: the module doc of
  `compiler/src/ir/refcount.rs` and the "Refcount insertion" section of
  `docs/design/ir.md`. Treat those as the spec. `docs/finalpush.md` records why the
  rewrite happened and what the five failed patches taught.

## The model, in one paragraph (details in the spec)

Every counted value is an **owner** (call/native results, aggregates, `Index` reads, block
params — holds one reference from birth) or a **view** (`Field`/`Elem`/`Cast`/`UnwrapOk`/
`UnwrapErr` results — holds nothing, aliases what its root owns). Liveness is computed
over **roots only**; a use of a view is a use of its root. Consuming a live owner is
preceded by retain, at last use it moves; consuming a view always retains; an owner is
released after its last use. All terminator bookkeeping sits **on the CFG edge** (retain
view args, then release owners not moved and not live into that successor); branch/switch
edges that need code get fresh edge blocks appended at the end of the function. Runtime
conventions the IR relies on: **natives consume (release) their args**, prim ops borrow,
`CallClosure` borrows its callee, a lambda's env param is borrowed and never released.

## Build / verify

```sh
cargo nextest run -p neon-compiler --test backend_run   # the real score; sanitizers on by default
cargo nextest run                                        # full suite
```

To inspect one program (the CLI needs a sysroot; the repo has no installed layout):

```sh
mkdir -p /tmp/neon-sysroot && cd /tmp/neon-sysroot
ln -sfn ~/projects/personal/neon2/target/debug/lib lib
ln -sfn ~/projects/personal/neon2/target/debug/include include
ln -sfn ~/projects/personal/neon2/stdlib stdlib
ln -sfn ~/projects/personal/neon2/runtime runtime
export NEON_SYSROOT=/tmp/neon-sysroot
cargo run -q -p neon-cli -- ir tests/lang/<prog>.neon            # IR after all passes
cargo run -q -p neon-cli -- compile tests/lang/<prog>.neon -o /tmp/x \
    --sanitize address --sanitize undefined                      # writes /tmp/x.c next to it
ASAN_OPTIONS=detect_leaks=1 /tmp/x
```

(`target/debug/lib` and `include` exist after any cargo build — the runtime build script
populates them.)

## Method — this is load-bearing

Every bug that fell in this effort was found by reading `neon ir` output or the generated
`.c` file; every failed fix started from reasoning about a pass in the abstract. **Read the
output before theorising.** And if a fix series plateaus (the graveyard was five patches
stuck at 150/153), stop patching: that is evidence of a design problem. Write the failed
attempts down in `docs/finalpush.md` so they are not re-walked.

If you touch `ir/refcount.rs`: re-read its module doc first, keep the spec (module doc +
`ir.md`) updated in the same commit, and know that `refcount/tests.rs` pins the two bug
shapes that caused the 2026-07-19 rewrite — a returned view must release its base, and a
view passed on a conditional edge must be retained on that edge only.

## Next work, in rough priority (details in `docs/finalpush.md` §Also outstanding)

1. **`==` ignores `Eq` impls.** `docs/decisions.md:348` says `==`/`!=` desugar to
   `Eq::eq` and comparisons to `Ord::cmp`; actually `check.rs` does a structural overlap
   test and lowering emits `PrimOp::Eq`, so a user's `impl Eq` is silently ignored by the
   operator. First decide which side moves (doc or implementation — "`==` is always
   structural" is defensible), record the decision in `docs/decisions.md`, then make them
   agree.
2. **Stdlib breadth.** 48 functions across 5 modules; no sort, no real file I/O, no math.
   Pattern is established: native in `runtime/src/rt.c` (decide borrow-vs-consume per arg
   and say so in a comment), `@native` signature in `stdlib/std/*.neon`, corpus tests.
3. **Stacktraces.** Unbuilt. Note `opt-release` passes `-fomit-frame-pointer`, which
   fights it — resolve that conflict as part of the design. See the error-design notes.

Add a corpus test under `tests/lang/` for anything you fix — goldens are diffed and
sanitizer reports fail tests ahead of the diff. If a golden looks wrong, verify against
source semantics before trusting it; two have been wrong before.
