use super::*;
use crate::lexer;
use crate::parser;

fn parse(src: &str) -> Module {
    let lexed = lexer::lex_full(src).expect("lexes");
    let (module, errors) = parser::parse(&lexed.tokens, src.len());
    assert!(errors.is_empty(), "{errors:?}");
    module.expect("parses")
}

/// `Debug` is derived, so it walks the tree independently of `ids.rs`. A branch
/// missed there shows up here as a surviving `UNSET` — which would otherwise be a
/// silent key collision between every expression the walk failed to reach.
fn debug_ids(m: &Module) -> (usize, usize) {
    let s = format!("{m:?}");
    let unset = s.matches(&format!("ExprId({})", u32::MAX)).count();
    let total = s.matches("ExprId(").count();
    (total, unset)
}

#[test]
fn every_expression_is_numbered() {
    let m = parse(
        r##"
        fn f(x: i64) -> i64 {
            let a = [1, 2, ..rest];
            let b = { name: "n", age: 1 + 2 };
            let c = (p) => p.field;
            let d = if x is i64 { x } else { 0 };
            let e = match x { 1 => "a", _ => "b" };
            while x > 0 { x = x - 1; }
            for i in a { io::println("#{i}") }
            let g = try? h(x) catch (err) { 0 };
            loop { break 1; }
            return d;
        }
        "##,
    );
    let (total, unset) = debug_ids(&m);
    assert!(total > 20, "expected a lot of expressions, saw {total}");
    assert_eq!(unset, 0, "{unset} of {total} expressions were left UNSET");
}

#[test]
fn ids_are_unique_and_contiguous() {
    let mut m = parse("fn f() -> i64 { let a = 1 + 2 * 3; a }");
    let n = number_exprs(&mut m);
    let s = format!("{m:?}");
    for i in 0..n {
        assert_eq!(
            s.matches(&format!("ExprId({i})")).count(),
            1,
            "ExprId({i}) should appear exactly once"
        );
    }
}

#[test]
fn numbering_is_stable_across_reformatting() {
    // Same tree shape must get the same ids, or `parse(format(src)) == parse(src)`
    // would fail for a reason that has nothing to do with formatting.
    let a = parse("fn f() -> i64 { 1 + 2 }");
    let b = parse("fn  f( )  ->  i64  {\n    1  +  2\n}");
    let mut a2 = a.clone();
    let mut b2 = b.clone();
    strip_spans(&mut a2);
    strip_spans(&mut b2);
    assert_eq!(a2, b2);
}

#[test]
fn nested_and_interpolated_expressions_are_numbered() {
    let m = parse(r##"fn f() { io::println("a #{1 + 2} b #{ g("x") }") }"##);
    let (total, unset) = debug_ids(&m);
    assert_eq!(unset, 0);
    assert!(
        total >= 8,
        "interpolation holes are expressions too, saw {total}"
    );
}
