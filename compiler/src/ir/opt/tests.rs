use crate::ir::lower::lower_module;
use crate::ir::opt::optimize;
use crate::ir::ssa::print;
use crate::typecheck::{check::check_module, Env};
use crate::{lexer, parser};

fn optimized(src: &str) -> String {
    let tokens = lexer::lex(src).expect("lexes");
    let (module, e) = parser::parse(&tokens, src.len());
    assert!(e.is_empty());
    let module = module.expect("parses");
    let mut env = Env::build(&module);
    assert!(env.errors().is_empty(), "{:?}", env.errors());
    let (result, errs) = check_module(&mut env, &module);
    assert!(errs.is_empty(), "{errs:?}");
    let mut program = lower_module(&env, &result, &module, &[]);
    optimize(&mut program);
    print::program(&program)
}

#[test]
fn constant_arithmetic_folds_to_a_single_constant() {
    let ir = optimized("fn f() -> i64 { 2 + 3 * 4 }");
    // 2 + 3*4 folds to 14, and the dead intermediate constants are removed.
    assert!(ir.contains("const.i64 14"), "{ir}");
    assert!(!ir.contains("prim.add"), "the add is folded away: {ir}");
    assert!(!ir.contains("prim.mul"), "the mul is folded away: {ir}");
}

#[test]
fn a_dead_pure_computation_is_removed() {
    let ir = optimized("fn f(x: f64) -> f64 { let unused = x * x; x }");
    // `unused` is pure and never read, so its multiply is dropped. `f64` because IEEE
    // arithmetic cannot trap; see the i64 companion below.
    assert!(!ir.contains("prim.mul"), "dead multiply removed: {ir}");
}

/// The i64 companion: an unread multiply is *kept*, because `i64` arithmetic traps on
/// overflow and deleting it would delete the trap.
///
/// This is the same rule `overflowing_constant_arithmetic_is_left_for_the_runtime` applies
/// to constant folding, which already refuses to fold `i64::MAX + 1` for exactly this
/// reason. The two passes disagreed: folding respected the trap while dead-code
/// elimination removed it, so `xs[10]` and `1 / 0` as statements ran clean and exited 0.
#[test]
fn a_dead_i64_computation_is_kept_because_it_can_trap() {
    let ir = optimized("fn f(x: i64) -> i64 { let unused = x * x; x }");
    assert!(ir.contains("prim.mul"), "trapping multiply kept: {ir}");
}

#[test]
fn an_effectful_call_is_kept_even_if_its_result_is_unused() {
    let ir = optimized(
        "@native(\"neon_io_println\") fn println(s: str) -> i64
         fn f() { let ignored = println(\"hi\"); }",
    );
    // println does I/O; its result is unused but the call must remain.
    assert!(ir.contains("neon_io_println"), "{ir}");
}

#[test]
fn overflowing_constant_arithmetic_is_left_for_the_runtime() {
    let ir = optimized("fn f() -> i64 { 9223372036854775807 + 1 }");
    // Folding would change behaviour if the runtime traps, so it is not folded.
    assert!(ir.contains("prim.add"), "overflow left unfolded: {ir}");
}

#[test]
fn simplify_cfg_collapses_a_folded_if_to_one_block() {
    // `if true { 1 } else { 2 }`: the branch folds, the dead arm and the join marshalling
    // blocks fall away, and single-predecessor merging fuses the rest into one block.
    let ir = optimized("fn f() -> i64 { if true { 1 } else { 2 } }");
    assert_eq!(ir.matches("block").count(), 1, "should collapse to a single block:\n{ir}");
    assert!(!ir.contains("jump"), "no residual forwarding:\n{ir}");
}

#[test]
fn a_constant_branch_folds_to_a_jump() {
    let ir = optimized("fn f() -> i64 { if true { 1 } else { 2 } }");
    // The condition is constant, so there is no branch and the dead arm is gone.
    assert!(!ir.contains("branch"), "constant branch folded: {ir}");
    assert!(ir.contains("const.i64 1"), "{ir}");
    assert!(!ir.contains("const.i64 2"), "dead arm removed: {ir}");
}

/// A call to a function that never returns survives DCE even though its result is unused.
///
/// `spin` is pure by every other measure the analysis has: f64 arithmetic and a comparison,
/// no natives, no indirect calls. It also never returns — `x * 0.0` is `0.0` forever, so
/// `x < 100.0` never goes false. DCE used to delete the call, and a program that must hang
/// printed its next line and exited 0. Non-termination is observable for exactly the reason
/// this file's effect analysis already treats a trap as observable.
///
/// The counterpart lives in `effects::tests`; this one pins the consequence rather than the
/// classification, because deleting the call is the damage.
#[test]
fn a_call_that_never_returns_is_not_deleted() {
    let ir = optimized(
        "fn spin(n: f64) -> f64 { let x = n; while x < 100.0 { x = x * 0.0; } x }
         fn main() { let unused = spin(1.0); }",
    );
    assert!(ir.contains("call @spin"), "the diverging call was deleted:\n{ir}");
}
