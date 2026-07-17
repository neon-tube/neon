use super::dispatch::{resolve, DispatchError, Resolution};
use super::env::Env;
use super::resolve::Scope;
use super::types::TyId;
use crate::{ast, lexer, parser};

fn parse(src: &str) -> ast::Module {
    let tokens = lexer::lex(src).expect("the fixture lexes");
    let (m, errs) = parser::parse(&tokens, src.len());
    assert!(errs.is_empty(), "parse errors in the fixture: {errs:?}");
    m.expect("the fixture parses")
}

fn env(src: &str) -> Env {
    let e = Env::build(&parse(src));
    assert!(e.errors().is_empty(), "fixture did not check: {:?}", e.errors());
    e
}

fn ty(e: &mut Env, src: &str) -> TyId {
    let m = parse(&format!("fn probe(x: {src}) {{ }}"));
    let ast::DeclKind::Fn(f) = &m.decls[0].kind else { unreachable!() };
    let scope = Scope::new(&[]);
    e.resolve(&scope, &f.params[0].ty)
}

const SHAPES: &str = "
    record Circle { r: i64 }
    record Square { s: i64 }
    record Tri { t: i64 }
    type Shape = Circle | Square
    protocol Area for T { fn area(v: T) -> i64 }
";

// ---- the common case ----

#[test]
fn a_single_impl_is_a_direct_call() {
    let mut e = env(&format!(
        "{SHAPES} impl Area for Circle {{ fn area(v: Circle) -> i64 {{ 1 }} }}"
    ));
    let circle = ty(&mut e, "Circle");
    let s = resolve(&mut e, "area", None, &[circle], None).expect("resolves");
    assert!(matches!(s.resolution, Resolution::Direct(_)));

    let i = e.solver.t.i64();
    assert_eq!(s.ret, i, "the return is the impl's, exactly — not erased");
}

#[test]
fn an_unknown_method_is_not_a_dispatch() {
    let mut e = env(SHAPES);
    let circle = ty(&mut e, "Circle");
    assert_eq!(
        resolve(&mut e, "perimeter", None, &[circle], None),
        Err(DispatchError::UnknownMethod("perimeter".into()))
    );
}

// ---- coverage, and the residual as the diagnostic ----

#[test]
fn a_receiver_with_no_impl_names_itself() {
    let mut e = env(&format!(
        "{SHAPES} impl Area for Circle {{ fn area(v: Circle) -> i64 {{ 1 }} }}"
    ));
    let tri = ty(&mut e, "Tri");
    let err = resolve(&mut e, "area", None, &[tri], None).unwrap_err();
    let DispatchError::NoImpl { uncovered, .. } = err else { panic!("{err:?}") };
    assert!(e.solver.is_equiv(uncovered, tri));
}

#[test]
fn a_partly_covered_receiver_names_only_the_gap() {
    // This is the thing a nominal system cannot do: the diagnostic is `Square`, not
    // "no impl of Area for Circle | Square".
    let mut e = env(&format!(
        "{SHAPES} impl Area for Circle {{ fn area(v: Circle) -> i64 {{ 1 }} }}"
    ));
    let shape = ty(&mut e, "Shape");
    let err = resolve(&mut e, "area", None, &[shape], None).unwrap_err();
    let DispatchError::NoImpl { uncovered, .. } = err else { panic!("{err:?}") };

    let square = ty(&mut e, "Square");
    assert!(e.solver.is_equiv(uncovered, square), "the gap is exactly Square");
}

// ---- spanning receivers ----

#[test]
fn a_spanning_receiver_is_a_switch_not_a_vtable() {
    let mut e = env(&format!(
        "{SHAPES}
         impl Area for Circle {{ fn area(v: Circle) -> i64 {{ 1 }} }}
         impl Area for Square {{ fn area(v: Square) -> i64 {{ 2 }} }}"
    ));
    let shape = ty(&mut e, "Shape");
    let s = resolve(&mut e, "area", None, &[shape], None).expect("resolves");
    let Resolution::Switch(arms) = &s.resolution else { panic!("{:?}", s.resolution) };
    assert_eq!(arms.len(), 2, "the applicable set is known here; no vtable is needed");
}

#[test]
fn the_return_is_the_union_of_the_applicable_returns() {
    // The impls disagree, so the answer is a union — as imprecise as the receiver
    // is, and no more. `Erased` has nowhere to enter.
    let mut e = env(
        "record Circle { r: i64 }
         record Square { s: i64 }
         type Shape = Circle | Square
         protocol Name for T { fn name(v: T) -> str }
         protocol Sized2 for T { fn sized(v: T) -> i64 }
         impl Name for Circle { fn name(v: Circle) -> str { \"c\" } }
         impl Name for Square { fn name(v: Square) -> str { \"s\" } }",
    );
    let shape = ty(&mut e, "Shape");
    let s = resolve(&mut e, "name", None, &[shape], None).expect("resolves");
    let st = e.solver.t.str();
    assert_eq!(s.ret, st, "both impls return str, so the call is as precise as a direct one");
}

// ---- specificity ----

#[test]
fn the_more_specific_impl_wins_a_nested_overlap() {
    let mut e = env(&format!(
        "{SHAPES}
         impl Area for Shape {{ fn area(v: Shape) -> i64 {{ 1 }} }}
         impl Area for Circle {{ fn area(v: Circle) -> i64 {{ 2 }} }}"
    ));
    let circle = ty(&mut e, "Circle");
    let s = resolve(&mut e, "area", None, &[circle], None).expect("resolves");
    let Resolution::Direct(id) = s.resolution else { panic!("{:?}", s.resolution) };

    let target = e.impls()[id.0].target.expect("a type target");
    assert!(e.solver.is_equiv(target, circle), "Circle's impl, not Shape's");
}

#[test]
fn a_widened_value_still_takes_the_specific_impl() {
    // decisions.md: specificity resolves per VALUE, not per static type. Widening a
    // Circle to Shape must not change which impl runs, so the switch has to carry
    // the Circle arm.
    let mut e = env(&format!(
        "{SHAPES}
         impl Area for Shape {{ fn area(v: Shape) -> i64 {{ 1 }} }}
         impl Area for Circle {{ fn area(v: Circle) -> i64 {{ 2 }} }}"
    ));
    let shape = ty(&mut e, "Shape");
    let circle = ty(&mut e, "Circle");
    let s = resolve(&mut e, "area", None, &[shape], None).expect("resolves");
    let Resolution::Switch(arms) = &s.resolution else { panic!("{:?}", s.resolution) };

    let has_circle_arm = arms.iter().any(|&(t, _)| e.solver.is_equiv(t, circle));
    assert!(has_circle_arm, "a Circle value must still find Circle's impl: {arms:?}");
}

// ---- ambiguity across protocols ----

#[test]
fn two_protocols_declaring_the_same_method_are_ambiguous() {
    let mut e = env(
        "record R { x: i64 }
         protocol A for T { fn go(v: T) -> str }
         protocol B for T { fn go(v: T) -> str }
         impl A for R { fn go(v: R) -> str { \"a\" } }
         impl B for R { fn go(v: R) -> str { \"b\" } }",
    );
    let r = ty(&mut e, "R");
    let err = resolve(&mut e, "go", None, &[r], None).unwrap_err();
    let DispatchError::Ambiguous { protocols, .. } = err else { panic!("{err:?}") };
    assert_eq!(protocols, vec!["A".to_string(), "B".to_string()], "the error names both");
}

#[test]
fn qualification_resolves_the_ambiguity() {
    // `A::go(r)` — pinned by the corpus.
    let mut e = env(
        "record R { x: i64 }
         protocol A for T { fn go(v: T) -> str }
         protocol B for T { fn go(v: T) -> str }
         impl A for R { fn go(v: R) -> str { \"a\" } }
         impl B for R { fn go(v: R) -> str { \"b\" } }",
    );
    let r = ty(&mut e, "R");
    let a = e.lookup_protocol(&[], &["A".to_string()]).expect("A exists");
    let s = resolve(&mut e, "go", Some(a), &[r], None).expect("resolves");
    assert_eq!(s.protocol, a);
    assert!(matches!(s.resolution, Resolution::Direct(_)));
}

#[test]
fn one_protocol_answering_is_not_ambiguous() {
    // Two protocols declare `go`, but only one has an impl for R.
    let mut e = env(
        "record R { x: i64 }
         protocol A for T { fn go(v: T) -> str }
         protocol B for T { fn go(v: T) -> str }
         impl A for R { fn go(v: R) -> str { \"a\" } }",
    );
    let r = ty(&mut e, "R");
    let s = resolve(&mut e, "go", None, &[r], None).expect("resolves");
    assert_eq!(s.protocol, e.lookup_protocol(&[], &["A".to_string()]).expect("A"));
}

// ---- the other resolution path ----

#[test]
fn a_rigid_receiver_resolves_against_the_bound() {
    // Inside `fn show[T](x: T) where T: Area`, T is opaque: no impl applies and
    // none ever will. The body is checked once, so the bound answers rather than
    // the impl registry. Conflating the two is how a library's errors end up
    // reported to its users.
    let mut e = env(&format!(
        "{SHAPES} impl Area for Circle {{ fn area(v: Circle) -> i64 {{ 1 }} }}"
    ));
    let n = e.solver.t.name("T");
    let t = e.solver.t.var(n);
    let s = resolve(&mut e, "area", None, &[t], None).expect("resolves");
    match &s.resolution {
        Resolution::Bound { param, .. } => assert_eq!(param, "T"),
        other => panic!("expected a bound, got {other:?}"),
    }
    let i = e.solver.t.i64();
    assert_eq!(s.ret, i, "the protocol's declared return, not an impl's");
}

// ---- receiverless ----

#[test]
fn a_method_with_no_subject_parameter_dispatches_on_the_expected_type() {
    // `fn make() -> T`. This is what the previous implementation got wrong: it
    // inferred T only from the return, could not propagate it, fell back to Erased,
    // and produced *_Any collections with 24-byte slots that push read as 8 — an
    // ASan overflow on every list::new(). A dispatch decision became a memory bug
    // four subsystems away.
    let mut e = env(
        "record Circle { r: i64 }
         protocol Make for T { fn make() -> T }
         impl Make for Circle { fn make() -> Circle { Circle { r: 0 } } }",
    );
    let circle = ty(&mut e, "Circle");
    let s = resolve(&mut e, "make", None, &[], Some(circle)).expect("resolves");
    assert!(matches!(s.resolution, Resolution::Direct(_)));
    assert_eq!(s.ret, circle);
}

#[test]
fn a_receiverless_method_with_no_expected_type_is_an_error() {
    let mut e = env(
        "record Circle { r: i64 }
         protocol Make for T { fn make() -> T }
         impl Make for Circle { fn make() -> Circle { Circle { r: 0 } } }",
    );
    assert_eq!(
        resolve(&mut e, "make", None, &[], None),
        Err(DispatchError::NoReceiver("make".into())),
        "no receiver and no expectation is a diagnostic, never a guess"
    );
}
