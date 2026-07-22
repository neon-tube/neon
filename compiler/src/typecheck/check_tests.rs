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

// ---- one error channel ----
//
// Checking used to report through two lists: what `check_all` returned, and `Env::errors`,
// where resolving a type annotation raises. A caller reading only the return value dropped
// every unknown type name written inside a function body, and the poison type that produced
// reached codegen. These tests pin the fix at the API, not at any one call site: whatever a
// caller does, reading the returned `Vec` is enough.

#[test]
fn an_unknown_type_in_a_body_is_in_the_returned_errors() {
    // The exact shape of the original bug. `Env::error` raises this while resolving the
    // annotation, so before the fix `errs` was empty and the program checked "clean".
    let m = parse("fn f() { let x: NoSuchType = 5; }");
    let mut env = Env::build(&m);
    assert!(env.errors().is_empty(), "the declarations resolve: {:?}", env.errors());

    let (_r, errs) = check_module(&mut env, &m);
    assert!(
        errs.iter().any(|e| e.kind == TypeErrorKind::Unknown("NoSuchType".into())),
        "the returned list must carry the resolution error: {errs:?}"
    );
}

#[test]
fn checking_leaves_no_errors_behind_in_the_env() {
    // The other half of the invariant. If `Env::errors` still held a copy, a caller
    // reading both would report it twice; if it held something the return value did not,
    // the two-channel bug would be back.
    let m = parse("fn f() { let x: NoSuchType = 5; }");
    let mut env = Env::build(&m);
    let (_r, errs) = check_module(&mut env, &m);
    assert!(!errs.is_empty(), "the fixture is meant to fail");
    assert!(
        env.errors().is_empty(),
        "check_module must drain the env's channel: {:?}",
        env.errors()
    );
}

#[test]
fn both_kinds_of_error_are_reported_once_each_in_span_order() {
    // Two resolution errors (raised via `Env::error`) and one checker error (a mismatch),
    // interleaved in the source. All three must appear, none twice, sorted by position.
    let src = "fn f() {
    let a: NoSuchType = 5;
    let b: i64 = \"s\";
    let c: AlsoMissing = 5;
}";
    let m = parse(src);
    let mut env = Env::build(&m);
    let (_r, errs) = check_module(&mut env, &m);

    let kinds: Vec<_> = errs.iter().map(|e| e.kind.clone()).collect();
    assert_eq!(
        kinds.iter().filter(|k| **k == TypeErrorKind::Unknown("NoSuchType".into())).count(),
        1,
        "reported exactly once: {kinds:?}"
    );
    assert_eq!(
        kinds.iter().filter(|k| **k == TypeErrorKind::Unknown("AlsoMissing".into())).count(),
        1,
        "reported exactly once: {kinds:?}"
    );
    assert_eq!(
        kinds.iter().filter(|k| matches!(k, TypeErrorKind::Mismatch { .. })).count(),
        1,
        "reported exactly once: {kinds:?}"
    );
    assert_eq!(errs.len(), 3, "no extras: {kinds:?}");

    let spans: Vec<usize> = errs.iter().map(|e| e.span.start).collect();
    let mut sorted = spans.clone();
    sorted.sort_unstable();
    assert_eq!(spans, sorted, "diagnostics come out in source order: {spans:?}");
    // And the middle one really is the checker's, so the sort interleaved the two phases
    // rather than concatenating them.
    assert!(
        matches!(kinds[1], TypeErrorKind::Mismatch { .. }),
        "the mismatch sits between the two unknowns: {kinds:?}"
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
fn a_bare_cast_that_can_fail_is_rejected() {
    // A narrowing cast might fail, so it must say what a mismatch does — `as!`
    // asserts (and is checked at runtime); bare `as` is for casts that cannot fail.
    let e = check("fn f(v: i64 | str) -> i64 { v as i64 }");
    assert!(e.iter().any(|k| matches!(k, TypeErrorKind::FallibleCast { .. })), "{e:?}");
    clean("fn f(v: i64 | str) -> i64 { v as! i64 }");
}

#[test]
fn a_soft_cast_yields_the_target_or_null() {
    clean("fn f(v: i64 | str) -> i64 | null { v as? i64 }");
    // A null-overlapping target makes the softened null ambiguous.
    let e = check("fn f(v: any) -> (str | null) | null { v as? (str | null) }");
    assert!(e.iter().any(|k| matches!(k, TypeErrorKind::SoftCastNullOverlap { .. })), "{e:?}");
}

#[test]
fn a_newtype_casts_to_and_from_its_representation() {
    clean("newtype Meter = f64  fn f(m: Meter) -> f64 { m as f64 }");
    clean("newtype Meter = f64  fn f(x: f64) -> Meter { x as Meter }");
}

#[test]
fn a_cast_between_unrelated_newtypes_is_rejected() {
    // Both wrap f64, but Meter and Second are disjoint: the bridge is one hop, not
    // two, so `m as Second` is not `m as f64 as Second`.
    let e = check("newtype Meter = f64  newtype Second = f64  fn f(m: Meter) -> Second { m as Second }");
    assert!(e.iter().any(|k| matches!(k, TypeErrorKind::ImpossibleCast { .. })), "{e:?}");
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

// ---- first-class calls ----

#[test]
fn an_arrow_typed_local_can_be_called() {
    clean("fn f(g: (i64) -> i64) -> i64 { g(1) }");
}

#[test]
fn a_first_class_call_checks_its_argument() {
    mismatch(r#"fn f(g: (i64) -> i64) -> i64 { g("s") }"#);
}

#[test]
fn a_first_class_call_has_the_arrows_return_type() {
    clean("fn f(g: (i64) -> str) -> str { g(1) }");
    mismatch("fn f(g: (i64) -> str) -> i64 { g(1) }");
}

#[test]
fn a_first_class_call_checks_arity() {
    let e = check("fn f(g: (i64) -> i64) -> i64 { g(1, 2) }");
    assert!(e.iter().any(|k| matches!(k, TypeErrorKind::Arity { .. })), "{e:?}");
}

#[test]
fn calling_a_non_function_is_rejected() {
    let e = check("fn f(x: i64) -> i64 { x(1) }");
    assert!(e.iter().any(|k| matches!(k, TypeErrorKind::NotCallable { .. })), "{e:?}");
}

#[test]
fn a_higher_order_function_passes_a_fn_by_name() {
    // The callee is a first-class value; the argument is a named fn used as a value.
    clean(
        "fn inc(n: i64) -> i64 { n + 1 }
         fn apply_it(g: (i64) -> i64, x: i64) -> i64 { g(x) }
         fn f() -> i64 { apply_it(inc, 5) }",
    );
}

#[test]
fn passing_the_wrong_fn_type_is_rejected() {
    mismatch(
        "fn takes_str(s: str) -> str { s }
         fn apply_it(g: (i64) -> i64, x: i64) -> i64 { g(x) }
         fn f() -> i64 { apply_it(takes_str, 5) }",
    );
}

#[test]
fn a_local_shadows_a_module_fn_when_called() {
    // `g` the parameter, not any `g` in the fn table.
    clean(
        "fn g(n: str) -> str { n }
         fn f(g: (i64) -> i64) -> i64 { g(1) }",
    );
}

// ---- lambdas: checking mode ----

#[test]
fn a_lambda_argument_infers_its_param_from_the_callee() {
    clean(
        "fn apply_it(g: (i64) -> i64, x: i64) -> i64 { g(x) }
         fn f() -> i64 { apply_it((x) => x + 1, 5) }",
    );
}

#[test]
fn a_lambda_body_is_checked_against_the_expected_return() {
    // Expected `(i64) -> i64`, but the body is a str.
    mismatch(
        r#"fn apply_it(g: (i64) -> i64, x: i64) -> i64 { g(x) }
           fn f() -> i64 { apply_it((x) => "s", 5) }"#,
    );
}

#[test]
fn a_lambda_param_takes_the_expected_type_not_a_narrower_one() {
    // Expected `(i64|str) -> str`, so `x: i64|str` inside — a str body is fine.
    clean(
        r#"fn g(f: (i64 | str) -> str) -> str { f(1) }
           fn f() -> str { g((x) => "hi") }"#,
    );
}

#[test]
fn a_lambda_bound_with_an_annotation_checks() {
    clean("fn f() -> i64 { let a: (i64) -> i64 = (x) => x + 1; a(33) }");
}

#[test]
fn a_lambda_param_annotation_lets_it_synthesize() {
    clean("fn f() -> i64 { let a = (x: i64) => x + 1; a(33) }");
}

#[test]
fn a_lambda_param_with_no_type_and_no_context_is_an_error() {
    // The example this design deliberately does not infer: a bare binding with no
    // annotation, disambiguated only by a later use. That is unification.
    let e = check("fn f() -> i64 { let a = (x) => x + 1; a(33) }");
    assert!(e.iter().any(|k| matches!(k, TypeErrorKind::LambdaParamNeedsType(_))), "{e:?}");
}

#[test]
fn a_closure_may_not_rebind_a_capture() {
    let e = check("fn f() -> i64 { let c = 0; let g = () => { c = c + 1; c }; g() }");
    assert!(e.iter().any(|k| matches!(k, TypeErrorKind::RebindCapture { .. })), "{e:?}");
}

#[test]
fn a_closure_may_rebind_its_own_local() {
    // A name introduced inside the closure is not a capture, so rebinding it is fine.
    clean("fn f() -> i64 { let g = () => { let n = 0; n = n + 1; n }; g() }");
}

#[test]
fn a_lambda_of_the_wrong_arity_is_rejected() {
    mismatch(
        "fn apply_it(g: (i64) -> i64, x: i64) -> i64 { g(x) }
         fn f() -> i64 { apply_it((a, b) => a, 5) }",
    );
}

#[test]
fn a_protocol_method_dispatches_without_importing_the_protocol() {
    // No `use` of Sized: dispatch searches every protocol, not only imported ones.
    // This is the property that must not regress into Rust-style trait-in-scope.
    clean(
        "record Buf { n: i64 }
         protocol Sized for T { fn len(v: T) -> i64 }
         impl Sized for Buf { fn len(v: Buf) -> i64 { v.n } }
         fn f(b: Buf) -> i64 { len(b) }",
    );
}

#[test]
fn a_module_fn_wins_over_a_protocol_method_of_the_same_name() {
    // Lexical first: the str-returning module `len` shadows the i64 protocol method,
    // so this returns str. If the method had won it would be a mismatch.
    clean(
        "record Buf { n: i64 }
         protocol Sized for T { fn len(v: T) -> i64 }
         impl Sized for Buf { fn len(v: Buf) -> i64 { v.n } }
         fn len(b: Buf) -> str { \"n\" }
         fn f(b: Buf) -> str { len(b) }",
    );
}

// ---- generic function calls ----

#[test]
fn a_generic_return_is_inferred_from_the_expected_type() {
    clean(
        "record List[T] {}
         @native(\"n\") fn new[T]() -> List[T]
         fn f() { let xs: List[i64] = new(); }",
    );
}

#[test]
fn a_generic_param_is_inferred_from_the_argument() {
    clean(
        "record List[T] {}
         @native(\"g\") fn get[T](xs: List[T], i: i64) -> T
         fn f(xs: List[str]) -> str { get(xs, 0) }",
    );
    mismatch(
        "record List[T] {}
         @native(\"g\") fn get[T](xs: List[T], i: i64) -> T
         fn f(xs: List[i64]) -> str { get(xs, 0) }",
    );
}

#[test]
fn a_turbofish_pins_the_type_argument() {
    clean(
        "record List[T] {}
         @native(\"n\") fn new[T]() -> List[T]
         fn f() -> List[i64] { new[i64]() }",
    );
    mismatch(
        "record List[T] {}
         @native(\"n\") fn new[T]() -> List[T]
         fn f() -> List[str] { new[i64]() }",
    );
}

#[test]
fn strict_inference_rejects_a_silent_widening() {
    // `push(xs, "s")` with xs: List[i64] pins T := i64 from the list, so the str
    // argument is a mismatch -- not a silent widening to List[i64|str].
    mismatch(
        "record List[T] {}
         @native(\"p\") fn push[T](xs: List[T], v: T) -> List[T]
         fn f(xs: List[i64]) { push(xs, \"s\"); }",
    );
    clean(
        "record List[T] {}
         @native(\"p\") fn push[T](xs: List[T], v: T) -> List[T]
         fn f(xs: List[i64]) { push(xs, 9); }",
    );
}

#[test]
fn widening_a_generic_is_explicit() {
    // The expected type sets T first, so the arguments conform to the wider list --
    // widening on request, via the annotation.
    clean(
        "record List[T] {}
         @native(\"p\") fn push[T](xs: List[T], v: T) -> List[T]
         fn f(xs: List[i64]) -> List[i64 | str] { push(xs, \"s\") }",
    );
    // A turbofish does the same.
    clean(
        "record List[T] {}
         @native(\"p\") fn push[T](xs: List[T], v: T) -> List[T]
         fn f(xs: List[i64]) { push[i64 | str](xs, \"s\"); }",
    );
}

// ---- interpolation desugars to to_string ----

const DISPLAY: &str = "
    protocol Display for T { fn to_string(v: T) -> str }
    impl Display for i64 { @native(\"i\") fn to_string(v: i64) -> str }
";

#[test]
fn an_interpolated_value_must_be_display() {
    clean(&format!("{DISPLAY} fn f(n: i64) -> str {{ \"n=#{{n}}\" }}"));
    // A record with no Display impl cannot be interpolated.
    let e = check(&format!(
        "{DISPLAY} record R {{ x: i64 }} fn f(r: R) -> str {{ \"#{{r}}\" }}"
    ));
    assert!(e.iter().any(|k| matches!(k, TypeErrorKind::NoImpl { .. })), "{e:?}");
}

// ---- comparison ----

#[test]
fn equality_needs_comparable_operands() {
    clean("fn f() -> bool { 1 == 2 }");
    clean("fn f() -> bool { :ok == :err }");   // both atoms, one domain
    clean("fn f(x: i64 | str, y: i64) -> bool { x == y }");  // overlap on i64
    // An atom and a string share no comparison domain.
    let e = check("fn f() -> bool { :ok == \"ok\" }");
    assert!(e.iter().any(|k| matches!(k, TypeErrorKind::Incomparable { .. })), "{e:?}");
    let e = check("fn f() -> bool { 1 == \"s\" }");
    assert!(e.iter().any(|k| matches!(k, TypeErrorKind::Incomparable { .. })), "{e:?}");
}

#[test]
fn ordering_needs_a_common_ordered_type() {
    clean("fn f() -> bool { \"a\" < \"b\" }");
    clean("fn f() -> bool { 1 < 2 }");
    // No common type to order.
    let e = check("fn f() -> bool { 1 < \"s\" }");
    assert!(e.iter().any(|k| matches!(k, TypeErrorKind::Incomparable { .. })), "{e:?}");
}

#[test]
fn ordering_needs_one_shape_even_when_the_types_match() {
    // Sharing a type is not enough: ordering a union would need a rank between its
    // arms, which is the cross-type order docs/decisions.md declines. `==` is still
    // fine on every one of these -- equality is total.
    for src in [
        "fn f(v: i64 | :none) -> bool { v < v }",
        "fn f(v: i64 | str) -> bool { v < v }",
        "fn f(v: i64 | null) -> bool { v < v }",
        "fn f(v: :lt | :gt) -> bool { v < v }",
    ] {
        let e = check(src);
        assert!(e.iter().any(|k| matches!(k, TypeErrorKind::Unordered { .. })), "{src}: {e:?}");
        clean(&src.replace(" < ", " == "));
    }
    // A single shape is ordered, aggregates included. (`List`/`Map` need the prelude,
    // which these fixtures do not load — operators/structural_ordering.neon and
    // operators/unordered_shapes_are_rejected.neon cover them.)
    clean("record P { x: i64, y: str } fn f(a: P, b: P) -> bool { a < b }");
    clean("fn f(a: (i64, str), b: (i64, str)) -> bool { a < b }");

    // Ordering recurses into parts: one unordered field or element is enough. A
    // one-level check passed all of these, and the backend then either emitted a
    // pointer comparison or called a null `cmp`.
    for src in [
        "record P { x: i64 | str } fn f(a: P, b: P) -> bool { a < b }",
        "record P { x: :ok | :err } fn f(a: P, b: P) -> bool { a < b }",
        "fn f(a: (i64, i64 | str), b: (i64, i64 | str)) -> bool { a < b }",
        // A record that reaches itself is a pointer, not a structure to walk.
        "record N { v: i64, next: N | null } fn f(a: N, b: N) -> bool { a < b }",
        // A bare type variable: nothing re-checks the instantiation, so it is refused
        // rather than deferred. See the note on `ordered_rec`.
        "fn smaller[T](a: T, b: T) -> bool { a < b }",
    ] {
        let e = check(src);
        assert!(e.iter().any(|k| matches!(k, TypeErrorKind::Unordered { .. })), "{src}: {e:?}");
    }
}

#[test]
fn a_null_comparison_needs_no_common_type() {
    // `x == null` is a tag test, not Eq, so it does not require i64 and null to be
    // comparable as values.
    clean("fn f(x: i64 | null) -> bool { x == null }");
    clean("fn f(x: i64 | null) -> bool { x != null }");
}

// ---- record literal fields ----

#[test]
fn a_record_literal_rejects_an_extra_field() {
    // Excess-property check: a fresh literal may not carry fields the target does
    // not declare -- that is a typo, not a widening.
    let e = check("fn g(o: { name: str }) {} fn f() { g({ name: \"x\", extra: 9 }); }");
    assert!(e.iter().any(|k| matches!(k, TypeErrorKind::NoField { .. })), "{e:?}");
}

#[test]
fn a_record_literal_may_omit_a_nullable_field() {
    // The optional-params rule: a missing nullable field defaults, so it is fine.
    clean("fn g(o: { a: i64, b: i64 | null }) {} fn f() { g({ a: 1 }); }");
}

#[test]
fn a_record_literal_may_not_omit_a_required_field() {
    let e = check("fn g(o: { a: i64, b: i64 }) {} fn f() { g({ a: 1 }); }");
    assert!(e.iter().any(|k| matches!(k, TypeErrorKind::MissingField(_))), "{e:?}");
}

#[test]
fn a_nominal_record_satisfies_a_structural_opts_type() {
    // A `| null` field is optional, so a nominal record that omits it -- and carries
    // an extra field besides -- still satisfies the structural type.
    clean(
        "record Preset { timeout: i64, label: str }
         fn show(o: { timeout: i64 | null, retries: i64 | null }) {}
         fn f() { show(Preset { timeout: 5, label: \"x\" }); }",
    );
}

#[test]
fn a_negated_field_is_not_optional() {
    // `!i64` admits null once resolved, but it was not written as `| null`, so the
    // field stays required: a record that omits it does not satisfy the type.
    mismatch(
        "record NoX { y: i64 }
         fn want(o: { x: !i64 }) {}
         fn f() { want(NoX { y: 1 }); }",
    );
}

#[test]
fn a_record_literal_checks_field_types() {
    let e = check("fn g(o: { a: i64 }) {} fn f() { g({ a: \"s\" }); }");
    assert!(e.iter().any(|k| matches!(k, TypeErrorKind::FieldTypeMismatch { .. })), "{e:?}");
}

// ---- iteration and indexing ----

const COLL: &str = "record List[T] {}  record Map[K, V] {}";

#[test]
fn a_for_loop_binds_the_element_type() {
    clean(&format!("{COLL} fn f(xs: List[i64]) -> i64 {{ let s = 0; for x in xs {{ s = x; }} s }}"));
    // The bound variable has the element type, so a str body is a mismatch.
    mismatch(&format!(
        "{COLL} fn f(xs: List[i64]) {{ for x in xs {{ let s: str = x; }} }}"
    ));
}

#[test]
fn iterating_a_non_collection_is_rejected() {
    let e = check(&format!("{COLL} fn f(n: i64) {{ for x in n {{ }} }}"));
    assert!(e.iter().any(|k| matches!(k, TypeErrorKind::NotIterable(_))), "{e:?}");
}

#[test]
fn a_list_index_yields_the_element() {
    clean(&format!("{COLL} fn f(xs: List[str]) -> str {{ xs[0] }}"));
    mismatch(&format!("{COLL} fn f(xs: List[i64]) -> str {{ xs[0] }}"));
}

#[test]
fn a_map_index_is_keyed_and_yields_the_value() {
    clean(&format!("{COLL} fn f(m: Map[str, i64]) -> i64 {{ m[\"k\"] }}"));
    // The key must match: a str-keyed map cannot be indexed by i64.
    mismatch(&format!("{COLL} fn f(m: Map[str, i64]) -> i64 {{ m[0] }}"));
}

// ---- generic record construction ----

#[test]
fn a_generic_record_infers_its_argument_from_the_fields() {
    clean("record Box[T] { item: T }  fn f() -> Box[i64] { Box { item: 7 } }");
    clean("record Box[T] { item: T }  fn f() { let b = Box { item: \"hi\" }; }");
}

#[test]
fn a_generic_record_with_two_uses_of_a_variable_must_agree() {
    clean("record Pair[T] { a: T, b: T }  fn f() { let p = Pair { a: 1, b: 2 }; }");
    mismatch("record Pair[T] { a: T, b: T }  fn f() { let p = Pair { a: 1, b: \"x\" }; }");
}

#[test]
fn a_generic_record_rejects_an_unknown_field() {
    let e = check("record Box[T] { item: T }  fn f() { let b = Box { item: 1, extra: 2 }; }");
    assert!(e.iter().any(|k| matches!(k, TypeErrorKind::NoField { .. })), "{e:?}");
}

// ---- where-clause bounds ----

const DISP2: &str = "
    protocol Display for T { fn to_string(v: T) -> str }
    record X { n: i64 }
    impl Display for X { @native(\"x\") fn to_string(v: X) -> str }
";

#[test]
fn a_generic_body_resolves_a_method_through_its_bound() {
    // T is opaque, so no impl applies; to_string resolves via `where T: Display`.
    clean(&format!("{DISP2} fn show[T](v: T) -> str where T: Display {{ to_string(v) }}"));
}

#[test]
fn a_bound_is_discharged_at_the_call_site() {
    clean(&format!(
        "{DISP2} fn show[T](v: T) -> str where T: Display {{ to_string(v) }}
         fn f() -> str {{ show(X {{ n: 1 }}) }}"
    ));
    // A type with no Display impl fails the bound.
    let e = check(&format!(
        "{DISP2} record Plain {{ n: i64 }}
         fn show[T](v: T) -> str where T: Display {{ to_string(v) }}
         fn f() -> str {{ show(Plain {{ n: 1 }}) }}"
    ));
    assert!(e.iter().any(|k| matches!(k, TypeErrorKind::UnsatisfiedBound { .. })), "{e:?}");
}

// ---- bound required, impl completeness, supertrait bounds ----

#[test]
fn a_rigid_method_call_requires_a_declared_bound() {
    // No `where T: Display`, so to_string(v) on a rigid T cannot resolve.
    let e = check(&format!("{DISP2} fn show[T](v: T) -> str {{ to_string(v) }}"));
    assert!(e.iter().any(|k| matches!(k, TypeErrorKind::UnsatisfiedBound { .. })), "{e:?}");
}

#[test]
fn an_impl_must_provide_every_required_method() {
    // A declaration-phase error, so read it from Env::build, not check_module.
    let m = parse(
        "protocol Named for T { fn name(v: T) -> str  fn greet(v: T) -> str }
         record Dog { tag: str }
         impl Named for Dog { fn name(v: Dog) -> str { v.tag } }",
    );
    let env = Env::build(&m);
    assert!(
        env.errors().iter().any(|e| matches!(e.kind, TypeErrorKind::ImplMissingMethod { .. })),
        "{:?}",
        env.errors()
    );
}

#[test]
fn a_supertrait_bound_satisfies_the_super_protocols_method() {
    // `where T: Ord` lets the body call Eq's method, since Ord requires Eq.
    clean(
        "protocol Eq for T { fn eq(a: T, b: T) -> bool }
         protocol Ord for T where T: Eq { fn cmp(a: T, b: T) -> i64 }
         fn same[T](a: T, b: T) -> bool where T: Ord { eq(a, b) }",
    );
}

// ---- resolved names ----
//
// The checker is the only pass that ever knows which binding a name means: the frame
// holding it is popped when the block ends, and nothing downstream can reconstruct the
// scope walk. These fix what it records, because the editor's jump-to-definition,
// find-references and rename are all this one map read forwards or backwards.

/// Parse, number, and check — numbering matters here in a way it does not for the
/// diagnostic tests, since an unnumbered module gives every expression the same id and
/// a map keyed by `ExprId` would collapse to one entry.
fn defs(src: &str) -> Vec<(String, super::result::DefSite)> {
    let mut m = parse(src);
    crate::ast::number_exprs(&mut m);
    let mut env = Env::build(&m);
    assert!(env.errors().is_empty(), "the fixture's declarations do not check: {:?}", env.errors());
    let (r, errs) = check_module(&mut env, &m);
    assert!(errs.is_empty(), "expected no errors, got {errs:?}");

    // Key by the source text the name was written as, not by `ExprId`: the ids are an
    // allocation order the test should not be pinned to.
    let mut out: Vec<_> = r
        .defs()
        .map(|(e, d)| {
            let span = span_of(&m, e).expect("every resolved name has a span");
            (src[span].to_string(), d.clone())
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.span.start.cmp(&b.1.span.start)));
    out
}

/// The span of one expression. The AST has no id-to-span index — nothing in the
/// compiler needs one — so the test builds the lookup it needs.
fn span_of(m: &ast::Module, want: crate::ast::ExprId) -> Option<std::ops::Range<usize>> {
    let mut found = None;
    ast::visit::each_expr(m, |e| {
        if e.id == want {
            found = Some(e.span.clone());
        }
    });
    found
}

#[test]
fn a_local_resolves_to_the_let_that_bound_it() {
    let d = defs("fn main() { let x = 1; let y = x; }");
    let (name, site) = d.iter().find(|(n, _)| n == "x").expect("`x` resolved");
    assert_eq!(name, "x");
    assert_eq!(site.kind, super::result::DefKind::Local);
    // The binding, not the use.
    assert_eq!(site.span.start, "fn main() { let ".len());
}

#[test]
fn a_parameter_is_distinguished_from_a_let() {
    let d = defs("fn f(n: i64) -> i64 { n }");
    let (_, site) = d.iter().find(|(n, _)| n == "n").expect("`n` resolved");
    assert_eq!(site.kind, super::result::DefKind::Param);
}

#[test]
fn the_inner_binding_wins() {
    // Two bindings of `x`; the use must resolve to the nearer one. This is the case a
    // naive by-name index gets wrong, and getting it wrong means rename edits the
    // wrong occurrences.
    let src = "fn main() { let x = 1; { let x = 2; let y = x; } }";
    let d = defs(src);
    let (_, site) = d.iter().find(|(n, _)| n == "x").expect("`x` resolved");
    assert_eq!(site.span.start, src.find("x = 2").expect("the inner binding is in the fixture"));
}

#[test]
fn a_call_resolves_to_the_function_it_names() {
    let d = defs("fn helper() -> i64 { 1 }\nfn main() { let n = helper(); }");
    let (_, site) = d.iter().find(|(n, _)| n == "helper").expect("`helper` resolved");
    assert_eq!(site.kind, super::result::DefKind::Fn);
}

#[test]
fn a_local_shadows_a_function_of_the_same_name() {
    // `path` tries the local first, so the recorded site must agree with the type the
    // same lookup produced. A jump that disagreed with hover would be worse than none.
    let src = "fn v() -> i64 { 1 }\nfn main() { let v = 2; let n = v; }";
    let d = defs(src);
    let (_, site) = d.iter().find(|(n, _)| n == "v").expect("`v` resolved");
    assert_eq!(site.kind, super::result::DefKind::Local);
}

#[test]
fn an_unresolved_name_records_nothing() {
    // Absent rather than wrong: an editor offering a jump to a name that does not
    // resolve would be pointing at whatever the last successful lookup found.
    let mut m = parse("fn main() { let n = nope; }");
    crate::ast::number_exprs(&mut m);
    let mut env = Env::build(&m);
    let (r, errs) = check_module(&mut env, &m);
    assert!(!errs.is_empty(), "the fixture is supposed to fail");
    assert!(r.defs().all(|(_, d)| d.kind != super::result::DefKind::Local));
}
