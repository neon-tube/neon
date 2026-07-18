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

#[test]
fn records_fields_and_tuples() {
    let ir = lower(
        "record Point { x: i64, y: i64 }
         fn mk() -> Point { Point { x: 1, y: 2 } }
         fn getx(p: Point) -> i64 { p.x }
         fn pair() -> (i64, i64) { (3, 4) }",
    );
    assert!(ir.contains("record Point{x: %0, y: %1}"), "{ir}");
    assert!(ir.contains("field %0.x"), "{ir}");
    assert!(ir.contains("tuple (%0, %1)"), "{ir}");
}

#[test]
fn a_list_literal_builds_a_list() {
    let ir = lower("opaque record List[T] {}\nfn nums() -> List[i64] { [5, 6] }");
    assert!(ir.contains("list [%0, %1]"), "{ir}");
}

#[test]
fn a_sum_type_match_becomes_a_decision_list() {
    let ir = lower(
        "record Circle { r: i64 }
         record Rect { w: i64, h: i64 }
         type Shape = Circle | Rect
         fn area(s: Shape) -> i64 {
             match s { is Circle => s.r, is Rect => 1 }
         }",
    );
    assert!(ir.contains("is_variant %0 Circle"), "{ir}");
    assert!(ir.contains("is_variant"), "{ir}");
    assert!(ir.contains("branch"), "{ir}");
}

#[test]
fn a_nullable_match_tests_null() {
    let ir = lower(
        "fn add_one(v: i64 | null) -> i64 { match v { is null => -1, n => n + 1 } }",
    );
    assert!(ir.contains("is_null %0"), "{ir}");
}

#[test]
fn a_literal_match_compares_equality() {
    let ir = lower("fn n(x: i64) -> i64 { match x { 1 => 10, _ => 0 } }");
    assert!(ir.contains("const.i64 1"), "{ir}");
    assert!(ir.contains("prim.eq"), "{ir}");
}

#[test]
fn a_loop_carries_reassigned_variables_as_block_args() {
    let ir = lower(
        "fn count() -> i64 {
             let i = 0;
             loop {
                 if i >= 10 { break i; }
                 i = i + 1;
             }
         }",
    );
    // The header takes the carried `i`, the back-edge and break pass it.
    assert!(ir.contains("jump block"), "{ir}");
    assert!(ir.contains("i64):"), "block param for carried i: {ir}");
}

#[test]
fn a_while_loop_tests_its_condition_in_the_header() {
    let ir = lower(
        "fn f() { let i = 0; while i < 3 { i = i + 1; } }",
    );
    assert!(ir.contains("prim.lt"), "{ir}");
    assert!(ir.contains("branch"), "{ir}");
}

#[test]
fn short_circuit_and_orelse_and_pipe() {
    let ir = lower(
        "fn both(a: bool, b: bool) -> bool { a and b }
         fn def(v: i64 | null) -> i64 { v orelse 0 }
         fn dbl(x: i64) -> i64 { x + x }
         fn piped(x: i64) -> i64 { x |> dbl() }",
    );
    // `and` short-circuits through blocks; `orelse` null-tests; pipe becomes a call.
    assert!(ir.contains("branch"), "and short-circuit: {ir}");
    assert!(ir.contains("is_null"), "orelse: {ir}");
    assert!(ir.contains("call @dbl(%0)"), "pipe: {ir}");
}

#[test]
fn a_cast_reinterprets() {
    let ir = lower("newtype Meter = f64\nfn m(x: f64) -> Meter { x as Meter }");
    assert!(ir.contains("cast %0"), "{ir}");
}

#[test]
fn try_and_throw_lower_to_tagged_result_control_flow() {
    let ir = lower(
        "record Bad { msg: str }
         fn risky(n: i64) throws Bad -> i64 { if n < 0 { throw Bad { msg: \"neg\" } } else { n } }
         fn use_propagate(n: i64) throws Bad -> i64 { try risky(n) }
         fn use_soften(n: i64) -> i64 { (try? risky(n)) orelse 0 }
         fn use_catch(n: i64) -> i64 { try risky(n) catch (e) { -1 } }",
    );
    // A throwing call is checked and, on error, jumps to a handler; throw terminates.
    assert!(ir.contains("is_err"), "{ir}");
    assert!(ir.contains("unwrap_ok"), "{ir}");
    assert!(ir.contains("throw %"), "propagate re-throws: {ir}");
}

#[test]
fn string_interpolation_builds_via_to_string_and_concat() {
    let ir = lower(
        "protocol Display for T { fn to_string(v: T) -> str }
         impl Display for i64 { @native(\"neon_i64_to_string\") fn to_string(v: i64) -> str }
         fn show(n: i64) -> str { \"n = #{n}\" }",
    );
    assert!(ir.contains("neon_i64_to_string"), "{ir}");
    assert!(ir.contains("neon_str_concat"), "{ir}");
}

#[test]
fn a_for_loop_indexes_the_list_with_a_carried_accumulator() {
    let ir = lower(
        "opaque record List[T] {}
         @native(\"neon_list_len\") fn len[T](xs: List[T]) -> i64
         fn total(xs: List[i64]) -> i64 {
             let sum = 0;
             for x in xs { sum = sum + x; }
             sum
         }",
    );
    assert!(ir.contains("neon_list_len"), "{ir}");
    assert!(ir.contains("index %"), "element read: {ir}");
    assert!(ir.contains("prim.lt"), "bound check: {ir}");
    assert!(ir.contains("prim.add"), "increment + accumulate: {ir}");
}

#[test]
fn a_lambda_captures_and_lowers_as_its_own_function() {
    let ir = lower(
        "fn adder(n: i64) -> (i64) -> i64 { (x) => x + n }",
    );
    // The lambda becomes its own function that unpacks `n` from the env; the parent
    // builds a closure capturing `n`.
    assert!(ir.contains("closure @lambda$"), "make closure: {ir}");
    assert!(ir.contains("fn @lambda$"), "lambda function: {ir}");
    assert!(ir.contains("elem %0.0"), "unpack capture from env: {ir}");
}

#[test]
fn user_impl_dispatch_calls_the_lowered_method() {
    let ir = lower(
        "protocol Area for T { fn area(s: T) -> i64 }
         record Sq { side: i64 }
         impl Area for Sq { fn area(s: Sq) -> i64 { s.side * s.side } }
         fn use_it(s: Sq) -> i64 { area(s) }",
    );
    // The impl method is lowered as its own function, and dispatch calls it by name.
    assert!(ir.contains("fn @Area$Sq$area"), "impl method lowered: {ir}");
    assert!(ir.contains("call @Area$Sq$area"), "dispatch calls it: {ir}");
}

#[test]
fn a_generic_function_is_specialised_per_instance() {
    let ir = lower(
        "fn identity[T](x: T) -> T { x }
         fn use_i(n: i64) -> i64 { identity(n) }
         fn use_s(s: str) -> str { identity(s) }",
    );
    // Two instances: identity$i64 and identity$str, each called by name.
    assert!(ir.contains("fn @identity$i64"), "i64 instance: {ir}");
    assert!(ir.contains("fn @identity$str"), "str instance: {ir}");
    assert!(ir.contains("call @identity$i64"), "{ir}");
    assert!(ir.contains("call @identity$str"), "{ir}");
    assert!(!ir.contains("'T"), "no type variable left after mono: {ir}");
}

#[test]
fn a_where_bound_is_discharged_to_the_concrete_impl() {
    let ir = lower(
        "protocol Display for T { fn to_string(v: T) -> str }
         impl Display for i64 { @native(\"neon_i64_to_string\") fn to_string(v: i64) -> str }
         fn show[T](v: T) -> str where T: Display { to_string(v) }
         fn use_it(n: i64) -> str { show(n) }",
    );
    // Inside show$i64, the bound `to_string` resolves to the i64 impl's native symbol.
    assert!(ir.contains("fn @show$i64"), "instance: {ir}");
    assert!(ir.contains("neon_i64_to_string"), "bound discharged to i64 impl: {ir}");
}
