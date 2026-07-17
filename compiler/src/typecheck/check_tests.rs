use super::check::check_module;
use super::env::{Env, TypeErrorKind};
use crate::{ast, lexer, parser};

fn parse(src: &str) -> ast::Module {
    let tokens = lexer::lex(src).expect("the fixture lexes");
    let (m, errs) = parser::parse(&tokens, src.len());
    assert!(errs.is_empty(), "parse errors in the fixture: {errs:?}");
    m.expect("the fixture parses")
}

/// The checker's diagnostics for `src`, with the declaration pass required clean —
/// a body checked against a signature that did not resolve reports the same mistake
/// twice, and this keeps the fixtures honest about which pass they are testing.
fn check(src: &str) -> Vec<TypeErrorKind> {
    let m = parse(src);
    let mut env = Env::build(&m);
    assert!(env.errors().is_empty(), "the fixture's declarations do not check: {:?}", env.errors());
    let (_r, errs) = check_module(&mut env, &m);
    errs.into_iter().map(|e| e.kind).collect()
}

fn messages(src: &str) -> Vec<String> {
    let m = parse(src);
    let mut env = Env::build(&m);
    let (_r, errs) = check_module(&mut env, &m);
    errs.iter().map(|e| e.to_string()).collect()
}

fn clean(src: &str) {
    let e = check(src);
    assert!(e.is_empty(), "expected no errors, got {e:?}");
}

fn mismatch(src: &str) {
    let e = check(src);
    assert!(
        e.iter().any(|k| matches!(k, TypeErrorKind::Mismatch { .. })),
        "expected a mismatch, got {e:?}"
    );
}

// ---- the keystone ----

#[test]
fn every_expression_gets_a_type() {
    let m = parse("fn f() -> i64 { let a = 1 + 2; let b = a; b }");
    let mut env = Env::build(&m);
    let (r, errs) = check_module(&mut env, &m);
    assert!(errs.is_empty(), "{errs:?}");
    // The map the previous implementation threw away, forcing lowering to re-derive
    // types, fail, and fall back to erasure.
    assert!(r.len() >= 5, "expected a type per expression, got {}", r.len());
}

// ---- assignability: one rule, most of the corpus ----

#[test]
fn a_literal_of_the_wrong_type_is_rejected() {
    mismatch(r#"fn f() { let x: i64 = "s"; }"#);
    mismatch("fn f() { let x: str = 1; }");
    clean("fn f() { let x: i64 = 1; }");
}

#[test]
fn null_is_not_assignable_to_a_non_nullable() {
    mismatch("fn f() { let x: i64 = null; }");
    clean("fn f() { let x: i64 | null = null; }");
}

#[test]
fn a_union_rejects_a_non_member() {
    mismatch("fn f() { let x: i64 | str = true; }");
    clean("fn f() { let x: i64 | str = 1; }");
    clean("fn f() { let x: i64 | str = \"s\"; }");
}

#[test]
fn an_atom_is_not_a_string() {
    mismatch(r#"fn f() { let x: str = :ok; }"#);
    mismatch("fn f() { let x: :ok = \"ok\"; }");
}

#[test]
fn an_atom_outside_the_union_is_rejected() {
    mismatch("fn f() { let x: :ok | :err = :other; }");
    clean("fn f() { let x: :ok | :err = :ok; }");
}

#[test]
fn a_variant_is_a_subtype_of_its_union() {
    clean(
        "record Circle { r: i64 }
         record Square { s: i64 }
         type Shape = Circle | Square
         fn f(c: Circle) { let s: Shape = c; }",
    );
}

#[test]
fn rebinding_to_the_wrong_type_is_rejected() {
    mismatch(r#"fn f() { let x: i64 = 1; x = "s"; }"#);
    clean("fn f() { let x: i64 = 1; x = 2; }");
}

#[test]
fn an_argument_must_fit_the_parameter() {
    mismatch(r#"fn g(n: i64) -> i64 { n }  fn f() { g("s"); }"#);
    clean("fn g(n: i64) -> i64 { n }  fn f() { g(1); }");
}

#[test]
fn a_return_must_fit_the_signature() {
    mismatch(r#"fn f() -> i64 { "s" }"#);
    clean("fn f() -> i64 { 1 }");
}

#[test]
fn an_annotation_widens_the_binding() {
    // `let x: i64|str = 1` binds the wider type, not `i64` — so the rebind is legal.
    clean(r#"fn f() { let x: i64 | str = 1; x = "s"; }"#);
}

#[test]
fn a_structural_parameter_checks_its_fields() {
    clean(
        "record Person { name: str, age: i64 }
         fn g(p: { name: str }) -> str { p.name }
         fn f(p: Person) { g(p); }",
    );
    mismatch(
        "record Person { name: i64 }
         fn g(p: { name: str }) -> str { p.name }
         fn f(p: Person) { g(p); }",
    );
}

#[test]
fn a_newtype_is_not_its_representation() {
    mismatch("newtype Id = i64\nfn f() { let x: Id = 1; }");
    mismatch("newtype Id = i64\nfn g(n: i64) -> i64 { n }\nfn f(i: Id) { g(i); }");
}

#[test]
fn newtype_siblings_do_not_mix() {
    mismatch("newtype A = i64\nnewtype B = i64\nfn g(a: A) {}\nfn f(b: B) { g(b); }");
}

#[test]
fn an_intersection_requires_every_operand() {
    clean(
        "record P { name: str, age: i64 }
         fn g(v: { name: str } & { age: i64 }) {}
         fn f(p: P) { g(p); }",
    );
    mismatch(
        "record P { name: str }
         fn g(v: { name: str } & { age: i64 }) {}
         fn f(p: P) { g(p); }",
    );
}

// ---- if without else ----

#[test]
fn an_if_used_as_a_value_needs_an_else() {
    let e = check("fn f() -> i64 { if true { 1 } }");
    assert!(e.iter().any(|k| matches!(k, TypeErrorKind::IfWithoutElse)), "{e:?}");

    let e = check("fn g(n: i64) {} fn f() { g(if true { 1 }); }");
    assert!(e.iter().any(|k| matches!(k, TypeErrorKind::IfWithoutElse)), "{e:?}");
}

#[test]
fn an_if_used_as_a_statement_does_not() {
    clean("fn f() { if true { let a = 1; } }");
}

#[test]
fn an_if_with_an_else_is_the_union_of_its_arms() {
    clean(r#"fn f() -> i64 | str { if true { 1 } else { "s" } }"#);
    mismatch(r#"fn f() -> i64 { if true { 1 } else { "s" } }"#);
}

// ---- exhaustiveness ----

#[test]
fn a_non_exhaustive_match_names_what_is_missing() {
    let m = messages(
        "record Circle { r: i64 }
         record Square { s: i64 }
         type Shape = Circle | Square
         fn f(s: Shape) -> i64 { match s { is Circle => 1 } }",
    );
    assert!(m.iter().any(|s| s.contains("not exhaustive")), "{m:?}");
    // The residual IS the diagnostic: it says Square, not "Shape".
    assert!(m.iter().any(|s| s.contains("Square")), "{m:?}");
}

#[test]
fn covering_every_arm_is_exhaustive() {
    clean(
        "record Circle { r: i64 }
         record Square { s: i64 }
         type Shape = Circle | Square
         fn f(s: Shape) -> i64 { match s { is Circle => 1, is Square => 2 } }",
    );
}

#[test]
fn a_wildcard_is_exhaustive() {
    clean("fn f(n: i64) -> i64 { match n { 1 => 1, _ => 0 } }");
}

#[test]
fn an_integer_literal_does_not_cover_i64() {
    // The trap the `exact` flag exists for: `1` is an i64, but it matches one i64.
    let e = check("fn f(n: i64) -> i64 { match n { 1 => 1 } }");
    assert!(e.iter().any(|k| matches!(k, TypeErrorKind::NotExhaustive { .. })), "{e:?}");
}

#[test]
fn a_nullable_match_must_handle_null() {
    let e = check("fn f(n: i64 | null) -> i64 { match n { is i64 => 1 } }");
    assert!(e.iter().any(|k| matches!(k, TypeErrorKind::NotExhaustive { .. })), "{e:?}");
    clean("fn f(n: i64 | null) -> i64 { match n { is i64 => 1, null => 0 } }");
}

#[test]
fn atoms_are_exhaustible_because_they_are_singletons() {
    clean("fn f(a: :ok | :err) -> i64 { match a { :ok => 1, :err => 0 } }");
    let e = check("fn f(a: :ok | :err) -> i64 { match a { :ok => 1 } }");
    assert!(e.iter().any(|k| matches!(k, TypeErrorKind::NotExhaustive { .. })), "{e:?}");
}

#[test]
fn a_guard_makes_an_arm_inexact() {
    // A guard can always decline, so a guarded arm covers nothing.
    let e = check("fn f(a: :ok | :err) -> i64 { match a { :ok if true => 1, :err => 0 } }");
    assert!(e.iter().any(|k| matches!(k, TypeErrorKind::NotExhaustive { .. })), "{e:?}");
}

// ---- casts ----

#[test]
fn a_cast_to_an_unrelated_type_is_rejected() {
    let e = check("fn f(n: i64) -> str { n as str }");
    assert!(e.iter().any(|k| matches!(k, TypeErrorKind::ImpossibleCast { .. })), "{e:?}");
}

#[test]
fn a_cast_that_narrows_is_fine() {
    clean("fn f(v: i64 | str) -> i64 { v as i64 }");
}

// ---- fields ----

#[test]
fn reading_a_field_nothing_has_is_rejected() {
    let e = check("record P { name: str }\nfn f(p: P) -> str { p.email }");
    assert!(e.iter().any(|k| matches!(k, TypeErrorKind::NoField { .. })), "{e:?}");
}

#[test]
fn a_field_read_has_the_fields_type() {
    clean("record P { name: str, age: i64 }\nfn f(p: P) -> str { p.name }");
    mismatch("record P { name: str, age: i64 }\nfn f(p: P) -> i64 { p.name }");
}

// ---- dispatch, through the checker ----

#[test]
fn a_receiver_with_no_impl_is_rejected_and_names_itself() {
    let m = messages(
        "record Circle { r: i64 }
         record Tri { t: i64 }
         protocol Area for T { fn area(v: T) -> i64 }
         impl Area for Circle { fn area(v: Circle) -> i64 { 1 } }
         fn f(t: Tri) -> i64 { area(t) }",
    );
    assert!(m.iter().any(|s| s.contains("no impl of `Area`")), "{m:?}");
    assert!(m.iter().any(|s| s.contains("Tri")), "{m:?}");
}

#[test]
fn an_ambiguous_call_names_both_protocols() {
    let m = messages(
        "record R { x: i64 }
         protocol A for T { fn go(v: T) -> str }
         protocol B for T { fn go(v: T) -> str }
         impl A for R { fn go(v: R) -> str { \"a\" } }
         impl B for R { fn go(v: R) -> str { \"b\" } }
         fn f(r: R) -> str { go(r) }",
    );
    assert!(m.iter().any(|s| s.contains("more than one protocol")), "{m:?}");
}

#[test]
fn a_local_fn_shadows_a_protocol_method() {
    // Lexical first. A module fn named `area` is not a dispatch at all.
    clean(
        "record Circle { r: i64 }
         protocol Area for T { fn area(v: T) -> i64 }
         impl Area for Circle { fn area(v: Circle) -> i64 { 1 } }
         fn area(c: Circle) -> i64 { 2 }
         fn f(c: Circle) -> i64 { area(c) }",
    );
}

#[test]
fn a_dispatched_call_has_the_impls_return_type() {
    clean(
        "record Circle { r: i64 }
         protocol Area for T { fn area(v: T) -> i64 }
         impl Area for Circle { fn area(v: Circle) -> i64 { 1 } }
         fn f(c: Circle) -> i64 { area(c) }",
    );
    // Not erased: the return is i64 exactly, so this mismatches.
    mismatch(
        "record Circle { r: i64 }
         protocol Area for T { fn area(v: T) -> i64 }
         impl Area for Circle { fn area(v: Circle) -> i64 { 1 } }
         fn f(c: Circle) -> str { area(c) }",
    );
}

// ---- names ----

#[test]
fn an_unknown_name_is_a_diagnostic_not_a_guess() {
    let e = check("fn f() -> i64 { nope }");
    assert!(e.iter().any(|k| matches!(k, TypeErrorKind::UnknownName(_))), "{e:?}");
}

#[test]
fn one_bad_expression_is_one_error() {
    // Poison satisfies nothing and is satisfied by nothing, and any check involving
    // it stays silent — so a cascade is one diagnostic, not twenty.
    let e = check("fn g(n: i64) -> i64 { n }\nfn f() -> i64 { g(nope) }");
    assert_eq!(e.len(), 1, "expected exactly one error, got {e:?}");
}

#[test]
fn arity_is_checked() {
    let e = check("fn g(a: i64, b: i64) -> i64 { a }\nfn f() -> i64 { g(1) }");
    assert!(e.iter().any(|k| matches!(k, TypeErrorKind::Arity { .. })), "{e:?}");
}
