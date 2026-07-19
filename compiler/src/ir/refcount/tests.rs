use crate::ir::lower::lower_module;
use crate::ir::refcount::insert;
use crate::ir::ssa::print;
use crate::typecheck::{check::check_module, Env};
use crate::{lexer, parser};

fn refcounted(src: &str) -> String {
    let tokens = lexer::lex(src).expect("lexes");
    let (module, e) = parser::parse(&tokens, src.len());
    assert!(e.is_empty());
    let module = module.expect("parses");
    let mut env = Env::build(&module);
    assert!(env.errors().is_empty(), "{:?}", env.errors());
    let (result, errs) = check_module(&mut env, &module);
    assert!(errs.is_empty(), "{errs:?}");
    let mut program = lower_module(&env, &result, &module, &[]);
    insert(&mut program);
    print::program(&program)
}

#[test]
fn scalars_get_no_refcounting() {
    let ir = refcounted("fn add(x: i64, y: i64) -> i64 { x + y }");
    assert!(!ir.contains("retain"), "no pointers, no retain: {ir}");
    assert!(!ir.contains("release"), "no pointers, no release: {ir}");
}

#[test]
fn an_unused_pointer_parameter_is_released() {
    // `s` (a str, pointer-backed) is never used, so its owned reference is released.
    let ir = refcounted("fn ignore(s: str) -> i64 { 0 }");
    assert!(ir.contains("release %0"), "the unused string is released: {ir}");
}

#[test]
fn a_returned_pointer_is_moved_not_released() {
    // The string is returned (moved out), so it must not be released.
    let ir = refcounted("fn id(s: str) -> str { s }");
    assert!(!ir.contains("release"), "a moved-out value is not released: {ir}");
}

#[test]
fn a_returned_view_retains_itself_and_releases_its_base() {
    // `w.s` is a view into what `w` owns: returning it must materialise a reference of
    // its own (retain), and `w` — kept alive until then only by the view — dies before
    // the return. This was the `Display$…$to_string` leak: the base was never released.
    let ir = refcounted(
        "record Wrap { s: str }
         fn get(w: Wrap) -> str { w.s }",
    );
    assert!(ir.contains("retain %1"), "the returned view is retained: {ir}");
    assert!(ir.contains("release %0"), "the view's base is released before the ret: {ir}");
    let retain = ir.find("retain %1").unwrap();
    let release = ir.find("release %0").unwrap();
    assert!(retain < release, "retain the view before releasing what it looks into: {ir}");
}

#[test]
fn a_view_passed_on_a_conditional_edge_is_retained_on_that_edge_only() {
    // `unwrap_err %2` flows into the handler on one edge of a branch. Its retain (and
    // the release of the tagged result it looks into) must sit on that edge — in a
    // block appended after the branch — not above the branch, where the other path
    // would run it too. That wrong-path retain was the standing 3-test leak.
    let ir = refcounted(
        "record E { msg: str }
         fn f() throws E -> i64 { throw E { msg: \"x\" } }
         fn g() -> i64 { try f() catch (e) { 0 } }",
    );
    let branch = ir.find("branch").expect("g branches on is_err");
    let retain = ir.find("retain").expect("the handed-on view is retained");
    assert!(
        retain > branch,
        "the retain sits in an edge block after the branch, not above it: {ir}"
    );
}

#[test]
fn a_pointer_used_twice_is_retained_once() {
    // `s` flows into two consuming positions (both native args), so one extra owned
    // reference is needed: a single retain.
    let ir = refcounted(
        "@native(\"neon_str_concat\") fn concat(a: str, b: str) -> str
         fn twice(s: str) -> str { concat(s, s) }",
    );
    assert_eq!(ir.matches("retain").count(), 1, "one duplicate consume -> one retain: {ir}");
}
