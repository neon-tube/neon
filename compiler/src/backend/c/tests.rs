use crate::backend::c::emit;
use crate::ir::lower::lower_module;
use crate::typecheck::{check::check_module, Env};
use crate::{lexer, parser};

fn emit_c(src: &str) -> String {
    let tokens = lexer::lex(src).expect("lexes");
    let (module, e) = parser::parse(&tokens, src.len());
    assert!(e.is_empty());
    let module = module.expect("parses");
    let mut env = Env::build(&module);
    assert!(env.errors().is_empty(), "{:?}", env.errors());
    let (result, errs) = check_module(&mut env, &module);
    assert!(errs.is_empty(), "{errs:?}");
    emit(&lower_module(&env, &result, &module))
}

#[test]
fn a_scalar_function_emits_c() {
    let c = emit_c("fn add(x: i64, y: i64) -> i64 { x + y }");
    assert!(c.contains("int64_t nl_add(int64_t _0, int64_t _1)"), "{c}");
    assert!(c.contains("neon_i64_add(_0, _1)"), "{c}");
    assert!(c.contains("return"), "{c}");
}

#[test]
fn a_native_call_and_main_entry_emit() {
    let c = emit_c(
        "@native(\"neon_io_println\") fn println(s: str)
         @native(\"neon_i64_to_string\") fn to_string(n: i64) -> str
         fn main() { println(to_string(42)); }",
    );
    assert!(c.contains("neon_i64_to_string(_0)"), "{c}");
    assert!(c.contains("neon_io_println(_1)"), "{c}");
    assert!(c.contains("int main(void)"), "{c}");
    assert!(c.contains("neon_rt_init();"), "{c}");
    assert!(c.contains("nl_main();"), "{c}");
}

#[test]
fn control_flow_uses_labels_and_gotos() {
    let c = emit_c("fn pick(c: bool, x: i64, y: i64) -> i64 { if c { x } else { y } }");
    assert!(c.contains("if (_0)"), "{c}");
    assert!(c.contains("goto block"), "{c}");
}
