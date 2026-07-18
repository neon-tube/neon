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
    let mut program = lower_module(&env, &result, &module);
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
fn a_pointer_used_twice_is_retained_once() {
    // `s` flows into two consuming positions (both native args), so one extra owned
    // reference is needed: a single retain.
    let ir = refcounted(
        "@native(\"neon_str_concat\") fn concat(a: str, b: str) -> str
         fn twice(s: str) -> str { concat(s, s) }",
    );
    assert_eq!(ir.matches("retain").count(), 1, "one duplicate consume -> one retain: {ir}");
}
