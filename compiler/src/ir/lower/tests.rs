use crate::ir::lower::lower_module;
use crate::ir::ssa::print;
use crate::typecheck::{check::check_module, Env};
use crate::{lexer, parser};

/// Check `src`, lower it, and return the printed IR of the whole program.
fn lower(src: &str) -> String {
    let tokens = lexer::lex(src).expect("lexes");
    let (module, perrs) = parser::parse(&tokens, src.len());
    assert!(perrs.is_empty(), "parse errors: {perrs:?}");
    let module = module.expect("parses");
    let mut env = Env::build(&module);
    assert!(env.errors().is_empty(), "declaration errors: {:?}", env.errors());
    let (result, errs) = check_module(&mut env, &module);
    assert!(errs.is_empty(), "check errors: {errs:?}");
    print::program(&lower_module(&env, &result, &module))
}

#[test]
fn arithmetic_and_a_direct_call() {
    let ir = lower("fn add(x: i64, y: i64) -> i64 { x + y }\nfn use_it() -> i64 { add(2, 3) }");
    assert_eq!(
        ir,
        "\
fn @add(%0 i64, %1 i64) -> i64 {
  block0:
    %2 = prim.add %0, %1
    ret %2
}

fn @use_it() -> i64 {
  block0:
    %0 = const.i64 2
    %1 = const.i64 3
    %2 = call @add(%0, %1)
    ret %2
}
"
    );
}

#[test]
fn an_if_becomes_blocks_with_a_join_argument() {
    let ir = lower("fn pick(c: bool) -> i64 { if c { 1 } else { 2 } }");
    assert_eq!(
        ir,
        "\
fn @pick(%0 bool) -> i64 {
  block0:
    branch %0, block1, block2
  block1:
    %2 = const.i64 1
    jump block3(%2)
  block2:
    %3 = const.i64 2
    jump block3(%3)
  block3(%1 i64):
    ret %1
}
"
    );
}

#[test]
fn a_let_binds_a_value_and_a_return_terminates() {
    let ir = lower("fn f(x: i64) -> i64 { let y = x + 1; return y; }");
    assert_eq!(
        ir,
        "\
fn @f(%0 i64) -> i64 {
  block0:
    %1 = const.i64 1
    %2 = prim.add %0, %1
    ret %2
}
"
    );
}

#[test]
fn a_void_function_returns_nothing() {
    let ir = lower("fn effectless(x: i64) { let y = x + 1; }");
    assert_eq!(
        ir,
        "\
fn @effectless(%0 i64) -> () {
  block0:
    %1 = const.i64 1
    %2 = prim.add %0, %1
    ret
}
"
    );
}

#[test]
fn native_calls_and_native_impl_dispatch() {
    let ir = lower(
        "protocol Display for T { fn to_string(v: T) -> str }
         impl Display for i64 { @native(\"neon_i64_to_string\") fn to_string(v: i64) -> str }
         @native(\"neon_io_println\") fn println(s: str)
         fn main() { println(to_string(42)); }",
    );
    // main lowers to two native calls, the inner dispatched to the i64 impl.
    assert!(ir.contains("native \"neon_i64_to_string\"(%0)"), "{ir}");
    assert!(ir.contains("native \"neon_io_println\"(%1)"), "{ir}");
}
