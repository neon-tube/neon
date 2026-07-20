//! Constant folding agrees with the runtime, for every operand pair.
//!
//! `ir::opt::fold_int` replaces a primitive on two constants with the constant it evaluates
//! to. That is only sound if the value it produces is the value the *runtime* would have
//! produced, and if it declines to fold precisely those operands the runtime would have
//! trapped on. Its doc comment asserts both. These harnesses discharge them.
//!
//! A model checker is the right tool here specifically because the claim is universally
//! quantified over 2^128 operand pairs and the interesting ones are a measure-zero subset:
//! `i64::MIN / -1`, `i64::MIN % -1`, division by zero, and the overflow boundary of each
//! wrapping operator. A unit test finds those only if someone thought to write them down;
//! a fuzzer finds them only by luck. Kani enumerates them by construction.
//!
//! `RUNTIME SEMANTICS` below is transcribed from `runtime/src/arith.c`. If that file
//! changes, these harnesses must change with it — they are the executable copy of it, and
//! a divergence between the two is exactly the miscompile being guarded against.

use neon_compiler::ir::opt::verify::{fold_bool, fold_int};
use neon_compiler::ir::ssa::{Op, PrimOp};

// ---- RUNTIME SEMANTICS: the reference model, from runtime/src/arith.c ----

/// `neon_i64_add`/`sub`/`mul` round-trip through `uint64_t`, which is how C is made to give
/// two's-complement wrapping rather than UB. In Rust that is `wrapping_*`.
fn rt_wrapping(op: PrimOp, x: i64, y: i64) -> i64 {
    match op {
        PrimOp::Add => x.wrapping_add(y),
        PrimOp::Sub => x.wrapping_sub(y),
        PrimOp::Mul => x.wrapping_mul(y),
        _ => unreachable!(),
    }
}

/// `neon_i64_div`/`rem` call `neon_trap` on a zero divisor and on `INT64_MIN / -1`, whose
/// true quotient is not representable. `None` is "the runtime traps here".
fn rt_div_rem(op: PrimOp, x: i64, y: i64) -> Option<i64> {
    if y == 0 || (x == i64::MIN && y == -1) {
        return None;
    }
    Some(match op {
        PrimOp::Div => x / y,
        PrimOp::Rem => x % y,
        _ => unreachable!(),
    })
}

// ---- helpers ----

// An `Op` must never be dropped inside a harness. This is the whole reason these helpers
// exist, and it is not an optimisation — it is the difference between seconds and never
// terminating.
//
// `Op::IsVariant` holds an `Option<Repr>`, and `Repr::Union(Vec<Repr>)` is self-referential.
// So the drop glue for `Op` is mutually recursive with the drop glue for `Repr`, and CBMC
// cannot bound it: it unwinds `drop_in_place::<Box<Repr>>` forever. Left to run, a harness
// as trivial as "`<` is the negation of `>=`" spent ten minutes at 100% CPU and was still
// unwinding at iteration 1498 when it was killed. Nothing about the *proof* is hard; the
// destructor is.
//
// `mem::forget` cuts that glue out of the reachable call graph entirely. Leaking is
// meaningless here — a harness is a symbolic path, not a program that runs — so this costs
// nothing and buys termination. Project to a scalar first, forget the `Op`, then assert.
//
// The same reasoning bans `assert_eq!` on an `Option<Op>` and `{:?}` in a panic message:
// both would need the `Op` as a whole value. Wrong-shape cases use a `&'static str` assert.
fn project<T>(r: Option<Op>, f: impl FnOnce(&Op) -> Option<T>) -> Option<T> {
    let out = match &r {
        Some(op) => {
            let projected = f(op);
            // A fold to the wrong constant shape is itself a bug, and one a silent
            // `None` would paper over.
            assert!(projected.is_some(), "op folded to an unexpected constant shape");
            projected
        }
        None => None,
    };
    std::mem::forget(r);
    out
}

fn folded_i64(op: PrimOp, x: i64, y: i64) -> Option<i64> {
    project(fold_int(op, x, y), |o| match o {
        Op::ConstI64(v) => Some(*v),
        _ => None,
    })
}

fn folded_bool(op: PrimOp, x: i64, y: i64) -> Option<bool> {
    project(fold_int(op, x, y), |o| match o {
        Op::ConstBool(v) => Some(*v),
        _ => None,
    })
}

/// Whether `fold_int` declined. Goes through `forget` for the same reason as the rest:
/// `Option::is_none` drops the `Op` in the `Some` case.
fn declines(op: PrimOp, x: i64, y: i64) -> bool {
    let r = fold_int(op, x, y);
    let none = r.is_none();
    std::mem::forget(r);
    none
}

/// `fold_bool`'s result, projected to a plain `bool` for the same reason as above.
fn folded_bool_op(op: PrimOp, x: bool, y: bool) -> Option<bool> {
    project(fold_bool(op, x, y), |o| match o {
        Op::ConstBool(v) => Some(*v),
        _ => None,
    })
}

// ---- the proofs ----

/// `+`, `-`, `*`: a fold, when it happens, is the wrapped value.
///
/// The folder uses `checked_*` and declines on overflow, while the runtime wraps. That is a
/// *missed* fold rather than a changed meaning, and this is what says so: whenever it does
/// fold, the answer matches `rt_wrapping`. The converse is deliberately not asserted —
/// declining is always sound, so requiring a fold would be pinning an optimisation, not a
/// correctness property.
/// One operator per harness, deliberately.
///
/// A single harness picking the operator nondeterministically makes the solver reason about
/// every op at once, and the whole query then costs whatever the *worst* op costs. Split,
/// `Add` and `Sub` come back in seconds; bundled with `Mul` they did not return in ten
/// minutes. Splitting also means the failure message names the operator.
fn wrapping_op_agrees(op: PrimOp) {
    let x: i64 = kani::any();
    let y: i64 = kani::any();
    if let Some(v) = folded_i64(op, x, y) {
        assert_eq!(v, rt_wrapping(op, x, y));
    }
}

#[kani::proof]
fn wrapping_add_agrees_with_the_runtime() {
    wrapping_op_agrees(PrimOp::Add);
}

#[kani::proof]
fn wrapping_sub_agrees_with_the_runtime() {
    wrapping_op_agrees(PrimOp::Sub);
}

/// `Mul` is the expensive one: CBMC bit-blasts a 64x64 multiplier, and equivalence between
/// two of them is the classic hard instance for a SAT solver. Kissat handles it far better
/// than the default MiniSat.
#[kani::proof]
#[kani::solver(kissat)]
fn wrapping_mul_agrees_with_the_runtime() {
    wrapping_op_agrees(PrimOp::Mul);
}

/// `/` and `%`: folds exactly the non-trapping operands, and never folds a trap away.
///
/// This is the strong direction, an `if and only if`. Folding a trapping pair would delete
/// a runtime abort the program was entitled to (or, worse, abort the *compiler* while
/// folding code that never executes); declining on a non-trapping pair would only cost an
/// optimisation, but the two together pin `checked_div`/`checked_rem` as the exact
/// complement of `neon_trap`'s guard, which is the property that makes the fold reviewable.
/// The trap boundary, over the full 64-bit input space.
///
/// This is the half that carries the weight, and it is deliberately separated from the
/// value check below. Folding a *trapping* pair would delete an abort the program was
/// entitled to, or abort the compiler while folding code that never runs; declining a
/// non-trapping pair would only cost an optimisation. Asserting both directions pins
/// `checked_div`/`checked_rem` as the exact complement of `neon_trap`'s guard.
///
/// Only the decision is asserted, not the quotient, so the solver never has to relate two
/// division circuits — see `div_and_rem_values_agree_for_small_divisors` for why that
/// matters and what is given up.
fn div_rem_declines_exactly_on_traps(op: PrimOp) {
    let x: i64 = kani::any();
    let y: i64 = kani::any();
    assert_eq!(folded_i64(op, x, y).is_some(), rt_div_rem(op, x, y).is_some());
}

#[kani::proof]
fn div_declines_exactly_on_traps() {
    div_rem_declines_exactly_on_traps(PrimOp::Div);
}

#[kani::proof]
fn rem_declines_exactly_on_traps() {
    div_rem_declines_exactly_on_traps(PrimOp::Rem);
}

/// The quotient itself — but only for `|x|, |y| < 256`, and that bound is the point.
///
/// **This harness does not cover the full input space, unlike every other one here.**
///
/// Unbounded it does not terminate. Asserting `folded == rt` makes CBMC build a division
/// circuit for each side and prove the miter unsat, and a 64-bit signed division miter is a
/// classically hard SAT instance: it ran past seven minutes under kissat and was killed.
/// Bounding only the divisor is not enough either — that still leaves a full-width dividend
/// and so still a 64-bit divider, and it also timed out at 400s. Both operands have to be
/// bounded, and then it verifies in about five seconds.
///
/// Keeping it bounded beats deleting it, because what it still catches is a *shape* bug —
/// folding `Div` with `%`, swapping the operands, folding to the wrong constant variant —
/// and those show up at `y = 3` as readily as at `y = 2^61`. What it cannot catch is a
/// value bug appearing only at large operands. Nothing plausible has that shape: both sides
/// invoke the same Rust operator, so the assertion is close to a tautology, and the
/// genuinely interesting operands (`0`, and `-1` against `i64::MIN`) are exactly the ones
/// `div_declines_exactly_on_traps` already covers over the full range.
///
/// The residual risk this leaves is Rust's `/` disagreeing with C's, which is not in scope
/// for Kani at all — `runtime/src/arith.c` is not part of the goto-binary. That gap is
/// closed by `tests/lang`, not here.
#[kani::proof]
fn div_and_rem_values_agree_for_small_operands() {
    let x: i64 = kani::any();
    let y: i64 = kani::any();
    kani::assume(x > -256 && x < 256);
    kani::assume(y > -256 && y < 256);

    let op = if kani::any() { PrimOp::Div } else { PrimOp::Rem };
    assert_eq!(folded_i64(op, x, y), rt_div_rem(op, x, y));
}

/// The bitwise operators are total, and bit-for-bit the C ones.
#[kani::proof]
fn bitwise_ops_are_total_and_exact() {
    let x: i64 = kani::any();
    let y: i64 = kani::any();
    let (op, want) = match kani::any::<u8>() % 3 {
        0 => (PrimOp::Band, x & y),
        1 => (PrimOp::Bor, x | y),
        _ => (PrimOp::Bxor, x ^ y),
    };
    assert_eq!(folded_i64(op, x, y), Some(want));
}

/// The comparisons are total, and agree with the signed comparison C emits.
#[kani::proof]
fn comparisons_are_total_and_exact() {
    let x: i64 = kani::any();
    let y: i64 = kani::any();
    let (op, want) = match kani::any::<u8>() % 6 {
        0 => (PrimOp::Eq, x == y),
        1 => (PrimOp::Ne, x != y),
        2 => (PrimOp::Lt, x < y),
        3 => (PrimOp::Le, x <= y),
        4 => (PrimOp::Gt, x > y),
        _ => (PrimOp::Ge, x >= y),
    };
    assert_eq!(folded_bool(op, x, y), Some(want));
}

/// The shifts are not folded at all — and this pins that, rather than leaving it implicit.
///
/// `fold_int` has no `Bsl`/`Bsr` arm, so both fall to the catch-all `None`. The backend
/// masks the amount to `& 63` (`backend/c.rs`), so a fold *could* be added, but it would
/// have to reproduce that mask exactly; `x << y` in Rust panics for `y >= 64` in debug and
/// is UB-adjacent otherwise, so the naive arm would abort the compiler on a shift the
/// program defines. This harness fails the moment someone adds a shift fold, which is the
/// point at which the mask needs proving rather than assuming.
#[kani::proof]
fn shifts_are_not_folded() {
    let x: i64 = kani::any();
    let y: i64 = kani::any();
    let op = if kani::any() { PrimOp::Bsl } else { PrimOp::Bsr };

    assert!(declines(op, x, y));
}

/// The unary and short-circuit ops never come through the binary integer folder.
///
/// `Neg`/`Bnot`/`Not` are unary and are handled (or not) by `fold_prim`; `And`/`Or` are the
/// boolean primitives. Reaching `fold_int` with any of them would mean `fold_prim` had
/// mis-dispatched, so "declines" is the property worth holding.
#[kani::proof]
fn non_binary_integer_ops_decline() {
    let x: i64 = kani::any();
    let y: i64 = kani::any();
    let op = match kani::any::<u8>() % 5 {
        0 => PrimOp::Neg,
        1 => PrimOp::Not,
        2 => PrimOp::Bnot,
        3 => PrimOp::And,
        _ => PrimOp::Or,
    };
    assert!(declines(op, x, y));
}

/// `fold_bool` is total on the four ops it claims, and declines everything else.
///
/// Only four inputs exist, so this is not a proof a test could not have written. It is here
/// because it is the other half of `fold_prim`'s dispatch, and having both under one
/// command is worth more than the four cases cost.
#[kani::proof]
fn bool_folding_is_total_and_exact() {
    let x: bool = kani::any();
    let y: bool = kani::any();

    assert_eq!(folded_bool_op(PrimOp::And, x, y), Some(x && y));
    assert_eq!(folded_bool_op(PrimOp::Or, x, y), Some(x || y));
    assert_eq!(folded_bool_op(PrimOp::Eq, x, y), Some(x == y));
    assert_eq!(folded_bool_op(PrimOp::Ne, x, y), Some(x != y));

    // Arithmetic on booleans is not a thing the folder should invent.
    assert!(folded_bool_op(PrimOp::Add, x, y).is_none());
    assert!(folded_bool_op(PrimOp::Lt, x, y).is_none());
}
