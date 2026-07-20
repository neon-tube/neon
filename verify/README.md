# Proving the compiler with Kani

[Kani](https://model-checking.github.io/kani/) is a bit-precise model checker for
Rust. Where `fuzz/` samples inputs, Kani enumerates *all* of them: a passing
harness is a proof over every value of every `kani::any()`, not a large number of
lucky draws.

`verify/` is a **detached workspace** — `verify/Cargo.toml` carries an empty
`[workspace]` table, and the root `Cargo.toml` lists `verify` under `exclude`.
`cargo build` and `cargo nextest run` at the repo root do not see it. Every
harness is behind `#[cfg(kani)]`, so a plain `cargo build` here compiles an empty
library and needs nothing from Kani.

## Requirements

Kani ships its own Rust toolchain and its own CBMC backend, so there is no
`rustup` component to add:

```sh
cargo install --locked kani-verifier
cargo kani setup                      # downloads CBMC; one time, ~1 GB
```

## Running

`verify/run.sh` is the entry point:

```sh
verify/run.sh                      # every harness
verify/run.sh wrapping_mul_agrees_with_the_runtime   # one harness
verify/run.sh '' --output-format=terse               # flags pass through
```

Eleven harnesses, about 18 seconds for the lot. If one appears to hang, read
**Why a trivial proof can hang** below before assuming the property is hard —
in this codebase it almost certainly is not.

## What is proved

One target so far: `ir::opt::fold_int` and `fold_bool`, the constant folder.

Folding is only sound if the constant it produces is the one the **runtime**
would have computed, and if it declines to fold exactly those operands the
runtime would have *trapped* on. `runtime/src/arith.c` is the other side of that
contract; `src/fold.rs` transcribes it into a reference model and checks the
folder against it.

| Harness | Property |
| --- | --- |
| `wrapping_{add,sub,mul}_agrees_with_the_runtime` | When the folder folds, the value equals `neon_i64_{add,sub,mul}`'s two's-complement wrap. The folder uses `checked_*` and declines on overflow; that is a *missed fold*, not a changed meaning, so only one direction is asserted. |
| `{div,rem}_declines_exactly_on_traps` | An `if and only if` over the full input space: folds precisely when `y != 0 && !(x == i64::MIN && y == -1)`. Folding a trapping pair would delete an abort the program was entitled to, or abort the *compiler* over code that never runs. |
| `div_and_rem_values_agree_for_small_operands` | The quotient and remainder themselves — **bounded to `\|x\|, \|y\| < 256`**, the one harness here that does not cover all inputs. See "Why a trivial proof can hang". |
| `bitwise_ops_are_total_and_exact` | `band`/`bor`/`bxor` always fold, bit-for-bit. |
| `comparisons_are_total_and_exact` | The six comparisons always fold, and agree with signed C comparison. |
| `bool_folding_is_total_and_exact` | `fold_bool` is total on `and`/`or`/`==`/`!=` and declines everything else. |
| `shifts_are_not_folded` | `bsl`/`bsr` are *not* folded today. Pinned deliberately — see below. |
| `non_binary_integer_ops_decline` | `Neg`/`Not`/`Bnot`/`And`/`Or` never come through the binary integer folder, so a `fold_prim` mis-dispatch is caught. |

This is the right shape of target for a model checker: the claim is universally
quantified over 2^128 operand pairs, and everything interesting lives in a
measure-zero subset — `i64::MIN / -1`, `i64::MIN % -1`, division by zero, and
each operator's overflow boundary. A unit test finds those only if somebody
thought to write them down; a fuzzer finds them only by luck.

`shifts_are_not_folded` is the one harness asserting an *absence*. `fold_int` has
no `Bsl`/`Bsr` arm, while the backend masks the amount (`(b & 63)`, `backend/c.rs`).
A shift fold could be added, but it would have to reproduce that mask exactly —
Rust's `<<` panics for amounts `>= 64` rather than masking, so the naive arm would
abort the compiler on a shift the language defines. The harness fails the moment
someone adds one, which is the point at which the mask wants proving.

## Why a trivial proof can hang

Two things dominate, and neither is the property being proved. Both were hit
while writing the first harness, so they are recorded here rather than
rediscovered.

### Never let an `Op` be dropped

`Op::IsVariant` holds an `Option<Repr>`, and `Repr::Union(Vec<Repr>)` is
self-referential. The drop glue for `Op` is therefore mutually recursive with the
drop glue for `Repr`, and CBMC cannot bound it — it unwinds
`drop_in_place::<Box<Repr>>` forever. A harness as trivial as "`<` is the negation
of `>=`" spent **ten minutes at 100% CPU** and was still unwinding at iteration
1498 when it was killed. With the destructor removed from the reachable graph the
same harness verifies in **2.8 seconds**.

So the helpers in `src/fold.rs` project the result to a plain `i64`/`bool` and
then `std::mem::forget` the `Op`. Leaking is meaningless in a model checker — a
harness is a symbolic path, not a program that runs — so this costs nothing.

The same reasoning bans `assert_eq!` on an `Option<Op>` and `{:?}` in a panic
message: both need the `Op` as a whole value, which drags in `String`, the
allocator and `core::fmt` besides. Wrong-shape cases use a `&'static str` assert.

**The `forget` does not trip a leak check, and cannot hide a bug.** Kani has no
memory-leak detector and no flag to enable one — CBMC implements
`--memory-leak-check`, but Kani never passes it, and `--no-memory-safety-checks`
governs dereferences, bounds and invalid addresses rather than reclamation. That
matches its model: a harness is a bounded symbolic entry point, not a process
with an exit at which an unreclaimed allocation would mean anything. Verified by
running a harness that forgets a `Box` and a `Vec` outright — 30 checks, none of
them about leaks, `VERIFICATION: SUCCESSFUL`.

Soundness here does not rest on that, though. Skipping the destructor cannot mask
a fault in `fold_int`: it allocates nothing, and its result is fully captured by
the scalar projected out before the `forget`. Anything the harness could observe
has already been read by the time the `Op` is dropped on the floor.

**Rule of thumb:** if a harness touches a compiler type with a recursive field,
project to scalars at the boundary and forget the original.

### One operator per harness

Picking the operator with `kani::any()` inside a single harness makes the solver
reason about every arm at once, and the query then costs whatever the *worst* arm
costs. Bundled with `Mul`, the wrapping harness did not return in ten minutes;
split, `Add` and `Sub` verify in under a second and `Mul` in about two. Splitting
also means a failure names the operator.

`Mul` additionally carries `#[kani::solver(kissat)]`. CBMC bit-blasts 64-bit
multiplication into a large circuit, and kissat handles it far better than the
default MiniSat.

### Assert the decision, not the value, where the value is a tautology

`div`/`rem` are the one place a property had to be split rather than just made
cheaper. Asserting `folded == reference` makes CBMC build a division circuit per
side and prove the miter unsat — a classically hard SAT instance. It ran past
seven minutes under kissat and was killed. Bounding only the *divisor* does not
help, because a full-width dividend still needs a 64-bit divider; that timed out
at 400s too.

The split that worked reflects where the value actually lies:

- **The trap boundary** (`{div,rem}_declines_exactly_on_traps`) is the property
  that carries the weight — fold a trapping pair and a runtime abort disappears.
  Asserting only `is_some()`, with no quotient, it verifies over the full 64-bit
  input space in **under a second**.
- **The quotient** is near-tautological: both sides invoke the same Rust
  operator, and whether Rust's `/` matches C's is not something Kani can see at
  all, since `runtime/src/arith.c` is not in the goto-binary. It is kept, bounded
  to `|x|, |y| < 256` (**5s**), because it still catches *shape* bugs — `Div`
  folded with `%`, swapped operands, the wrong constant variant — and those show
  up at `y = 3` as readily as at `y = 2^61`.

The operands that make division interesting — `0`, and `-1` against `i64::MIN` —
are exactly the ones the unbounded trap-boundary harness covers.

## These harnesses have been checked against a mutant

A proof that passes vacuously is worse than no proof. Replacing `checked_div`
with `wrapping_div` in `fold_int` — the exact miscompile the harness exists to
prevent, folding `i64::MIN / -1` instead of leaving the trap — makes
`div_declines_exactly_on_traps` fail. Worth redoing after any substantial change
to the helpers in `src/fold.rs`, since a mistake there (forgetting the wrong
value, asserting nothing) would fail open.

## Adding a target

Kani suits **pure functions over scalars with a small, total contract**. It does
not suit anything walking the `Env` type graph, a `HashMap`, or a parse tree —
those are heap-shaped and unbounded, and belong in `compiler/tests/` or `fuzz/`.

Good next candidates, in rough order of value:

- `backend/c.rs`'s shift emission (`(b & 63)`) against the runtime, which would
  also retire `shifts_are_not_folded`.
- `neon_i64_neg`'s `wrapping_neg` against `fold_prim`'s `Neg` arm — currently
  argued in a doc comment rather than checked.
- Span arithmetic in the lexer, if it is ever made to do anything but slice.

Harnesses outside `src/` can only see `pub` items. `fold_int` and `fold_bool` are
private to the pass; `ir::opt::verify` is a `#[doc(hidden)]` module of thin
forwarding wrappers that exists only for this crate. Prefer extending it over
widening anything to `pub` outright.
