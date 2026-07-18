# Final push: refcount correctness

State as of 2026-07-19. **692/695 suite, 150/153 backend under ASan+UBSan, zero
use-after-free, zero UB.** Three leaks remain, and they are one bug.

## How to measure

```
cargo nextest run -p neon-compiler --test backend_run
```

Sanitizers are **on by default** — every corpus program compiles with
`-fsanitize=address,undefined` and runs with leak detection, and a sanitizer report
fails the test ahead of the output diff. This is not optional decoration. The corpus
ran green over a genuine stack-buffer-overflow (a value handed to `neon_map_set`
uncoerced, so the witness memcpy'd 32 bytes out of an 8-byte double — the graveyard's
"24-byte slots read as 8", again). A passing suite without sanitizers proves the
answers look right, nothing more.

To look at one program:

```
neon compile tests/lang/<prog>.neon -o /tmp/x --sanitize address --sanitize undefined
ASAN_OPTIONS=detect_leaks=1 /tmp/x
```

`neon ir <file>` is the other half. Every bug below was found by reading the IR or the
generated C, not by reasoning about the pass.

## The ownership model

The gate into `refcount::insert` is `Repr::is_counted` — true for a counted pointer, or
an aggregate with one anywhere inside. It used to be `is_pointer`, which left every
aggregate untracked: a union, record or tuple holding a string or list was never
released. Its parts were counted when a witness walked them, but a value in a local
simply leaked.

The concept that was missing is a **view**. `Field`, `Elem`, `Cast`, `UnwrapOk` and
`UnwrapErr` do not produce a fresh reference — they hand back a look into what their
operand owns. `base_of` records the derivation. The rules:

1. A view is **never released** on its own. Releasing one destroys what its base still
   holds.
2. **Consuming** a view retains it — it must materialise a reference for whoever takes
   it. This applies at terminators too: a block argument, a return and a throw all
   consume.
3. **Borrowing** a view marks its base live, unconditionally. Guarding that behind the
   release had `release %0` float above the very calls using a view into `%0`.
4. The **root** of a projection chain is released once the last view into it dies.
   Without this a base kept alive only by views is never released at all — `sum(node)`
   read `value` and `next` out of a node and then leaked the node.
5. A lambda's **environment parameter is borrowed**, never released. The closure value
   owns it and may be called again.
6. `CallClosure` **borrows** its callee. Calling a closure reads it; it does not destroy
   it. Modelling it as consuming meant the reference was handed to a call that never
   released it, and every closure leaked its environment.
7. `Index` is **not** a view — `emit_index` retains what it reads, so that result owns
   itself.
8. Values dying on a block boundary are released at the top of the successor, for values
   live out of *every* predecessor, excluding block arguments (ownership moved to the
   parameter that received them).

Rule 8 is the one under suspicion. See below.

## What remains

Three programs leak: `errors/throw_from_catch`,
`strings/utf8_find_is_byte_offset`, `strings/utf8_slice_splits_sequence`. One cause — a
view passed as a block argument on a **conditional** edge:

```
%8 = unwrap_err %6
retain %8
branch %7, block2(%8), block3
```

The retain fires whether or not the handler runs, so the ok path leaks it. `block3`
correspondingly reaches `%9 = unwrap_ok %6; retain %9; jump block1(%9)` and never
releases `%6`.

## Five attempts, none better than 150

Recorded so they are not re-walked. Scores are `passing/153` and sanitizer kinds.

| Attempt | Result |
| --- | --- |
| Retain at the receiving block's parameter, when every incoming edge fills it with a view | 149, **3 UAF** |
| Split argument-carrying edges into their own blocks so the retain sits on the edge taken | 150, 3 leaks (no change) |
| After splitting, also release the view's base where the view escapes | 150, 3 leaks (guard never fired: `with_bases` marks the base live-out) |
| Same, dropping the liveness guard | 149, **4 UAF** (edge block and handler both free it) |
| Same, gated on the successor's `live_in` | 149, **4 UAF** (the handler's release comes from rule 8, not from `live_in`) |
| Replace rule 8 entirely with an edge-precise release at the end of single-successor blocks, keyed on `live_out` | 140, 0 UAF — **sound but catches less** |

Earlier dead ends from the same work:

- Widening the gate to `is_counted` **alone**, without the view concept: 23 leaks became
  9 use-after-frees.
- Blanket-retaining every projection result: regressed closures, 12 → 24 failures,
  because a closure environment's captures get double-counted on unpack.

## The hypothesis

Two mechanisms race to free the same value: the rule-8 boundary release and the view/base
release. Each of the five attempts shifted work between them instead of removing the
overlap. The telling result is the last one — replacing rule 8 with an edge-precise rule
was *sound* (0 UAF) but caught less (140). So rule 8 is load-bearing for cases the edge
rule cannot see, and the two need **unifying** rather than patching: one placement
mechanism derived from a single liveness answer, not a last-use rule plus a boundary rule
plus a base rule interacting.

This is a redesign of release placement, not another patch. Five patches is decent
evidence of that.

## Next step

Read `docs/design/ir.md`'s Perceus description first and check whether it already
specifies where releases belong. If it does, the implementation has drifted and the doc
is the spec to rebuild against. That is a reading task, and it is the right thing to do
with fresh context rather than at the end of a long session.

## Also outstanding, unrelated

- **`==` ignores `Eq` impls.** `docs/decisions.md:348` says `==`/`!=` desugar to
  `Eq::eq` and the comparisons to `Ord::cmp`. They do not: `check.rs` does a structural
  overlap test returning `bool`, and lowering emits `PrimOp::Eq`. A user who writes
  `impl Eq for MyType` gets a callable `eq(a, b)` that `a == b` ignores. Either the doc
  or the implementation has to move — "`==` is always structural equality" is defensible,
  just not what is written down.
- Return overloading **does** work: `dispatch.rs` falls back to the expected type when no
  parameter mentions the subject, so `fn make() -> T` resolves from context.
- Stdlib breadth: 48 functions across 5 modules. No sort, no real file I/O, no math.
- Stacktrace is still unbuilt, and `opt-release` passes `-fomit-frame-pointer`, which
  fights it.
