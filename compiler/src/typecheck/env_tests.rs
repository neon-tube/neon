use super::env::{Env, TypeErrorKind};
use super::resolve::Scope;
use super::types::TyId;
use crate::{ast, lexer, parser};

fn parse(src: &str) -> ast::Module {
    let tokens = lexer::lex(src).expect("the fixture lexes");
    let (m, errs) = parser::parse(&tokens, src.len());
    assert!(errs.is_empty(), "parse errors in the fixture {src:?}: {errs:?}");
    m.expect("the fixture parses")
}

fn env(src: &str) -> Env {
    Env::build(&parse(src))
}

/// Resolve a type written as source, in the root module.
fn ty(e: &mut Env, src: &str) -> TyId {
    let m = parse(&format!("fn probe(x: {src}) {{ }}"));
    let ast::DeclKind::Fn(f) = &m.decls[0].kind else { unreachable!("the fixture is a fn") };
    let scope = Scope::new(&[]);
    e.resolve(&scope, &f.params[0].ty)
}

fn errors(e: &Env) -> Vec<String> {
    e.errors().iter().map(|d| d.to_string()).collect()
}

fn kinds(e: &Env) -> Vec<TypeErrorKind> {
    e.errors().iter().map(|d| d.kind.clone()).collect()
}

fn assert_clean(e: &Env) {
    assert!(e.errors().is_empty(), "unexpected diagnostics: {:?}", errors(e));
}

// ---- primitives and the obvious constructors ----

#[test]
fn primitives_and_unions_resolve() {
    let mut e = env("");
    let i = ty(&mut e, "i64");
    let s = ty(&mut e, "str");
    let u = ty(&mut e, "i64 | str");
    assert_clean(&e);

    let expect = e.solver.t.union(i, s);
    assert!(e.solver.is_equiv(u, expect));
    assert!(e.solver.is_subtype(i, u));
}

#[test]
fn any_is_top_not_a_marker() {
    let mut e = env("");
    let a = ty(&mut e, "any");
    let top = e.solver.t.any();
    assert_eq!(a, top);
    for src in ["i64", "str", ":ok", "(i64) -> i64", "{ x: i64 }"] {
        let t = ty(&mut e, src);
        assert!(e.solver.is_subtype(t, a), "`any` is inhabited by every {src}");
    }
    assert_clean(&e);
}

#[test]
fn atoms_null_tuples_and_arrows() {
    let mut e = env("");
    let ok = ty(&mut e, ":ok");
    let n = ty(&mut e, "null");
    let pair = ty(&mut e, "(i64, str)");
    let f = ty(&mut e, "(i64) -> str");
    assert_clean(&e);

    let meet = e.solver.t.intersect(ok, n);
    assert!(e.solver.is_empty(meet));
    let meet = e.solver.t.intersect(pair, f);
    assert!(e.solver.is_empty(meet), "a tuple is never an arrow");
}

#[test]
fn an_unknown_name_is_one_diagnostic_and_a_poison() {
    let mut e = env("");
    let t = ty(&mut e, "Nope | i64");
    assert!(e.is_error(t), "poison propagates to the top of the spec");
    assert_eq!(kinds(&e), vec![TypeErrorKind::Unknown("Nope".into())]);
}

#[test]
fn the_poison_satisfies_nothing_and_is_satisfied_by_nothing() {
    let mut e = env("");
    let bad = ty(&mut e, "Nope");
    assert!(e.is_error(bad));
    for src in ["i64", "str", ":ok", "{ x: i64 }"] {
        let t = ty(&mut e, src);
        assert!(!e.solver.is_subtype(bad, t), "poison is not a subtype of {src}");
        assert!(!e.solver.is_subtype(t, bad), "{src} is not a subtype of poison");
    }
    assert!(!e.solver.is_empty(bad), "poison is not `never`");
}

// ---- negation ----

#[test]
fn user_negation_does_not_contain_the_absent_marker() {
    let mut e = env("record Person { name: str }");
    // `!i64` is a set of values. `undef` marks a record field as absent and is not
    // a value: if it leaked in, a record with no `name` would satisfy
    // `{ name: !i64 }`.
    let not_i64 = ty(&mut e, "!i64");
    let undef = e.solver.t.undef();
    let leak = e.solver.t.intersect(not_i64, undef);
    assert!(e.solver.is_empty(leak), "`!i64` must not contain undef");

    let any = e.solver.t.any();
    assert!(e.solver.is_subtype(not_i64, any), "`!T` stays inside the value lattice");
    assert_clean(&e);
}

#[test]
fn negation_still_complements_within_the_value_lattice() {
    let mut e = env("");
    let i = ty(&mut e, "i64");
    let not_i = ty(&mut e, "!i64");
    let meet = e.solver.t.intersect(i, not_i);
    assert!(e.solver.is_empty(meet));

    let join = e.solver.t.union(i, not_i);
    let any = e.solver.t.any();
    assert!(e.solver.is_equiv(join, any), "`i64 | !i64` is exactly `any`");

    let s = ty(&mut e, "str");
    assert!(e.solver.is_subtype(s, not_i));
    assert_clean(&e);
}

#[test]
fn a_missing_field_does_not_satisfy_a_negated_field_type() {
    let mut e = env("record Empty { }\nrecord Named { name: str }");
    let wants = ty(&mut e, "{ name: !i64 }");
    let empty = ty(&mut e, "Empty");
    let named = ty(&mut e, "Named");
    assert_clean(&e);

    assert!(e.solver.is_subtype(named, wants), "a str name is not an i64");
    assert!(!e.solver.is_subtype(empty, wants), "an absent name is not a `!i64` name");
}

// ---- aliases ----

#[test]
fn an_alias_is_transparent_and_chains() {
    let mut e = env("type Count = i64\ntype Tally = Count\ntype Maybe = i64 | null");
    let c = ty(&mut e, "Count");
    let t = ty(&mut e, "Tally");
    let i = ty(&mut e, "i64");
    let m = ty(&mut e, "Maybe");
    let u = ty(&mut e, "i64 | null");
    assert_clean(&e);

    assert_eq!(c, i, "an alias introduces a name, not a type");
    assert_eq!(t, i);
    assert!(e.solver.is_equiv(m, u));
}

#[test]
fn a_generic_alias_substitutes_its_arguments() {
    let mut e = env("type Pair[T] = (T, T)");
    let p = ty(&mut e, "Pair[i64]");
    let expect = ty(&mut e, "(i64, i64)");
    assert_clean(&e);
    assert_eq!(p, expect);
}

#[test]
fn a_recursive_plain_alias_is_rejected_and_points_at_mu() {
    let e = env("record Box[T] { item: T }\ntype Tree = :leaf | Box[Tree]");
    assert_eq!(kinds(&e), vec![TypeErrorKind::RecursiveAlias("Tree".into())]);
    assert!(errors(&e)[0].contains("mu type"), "the diagnostic names the binder");
}

#[test]
fn aliases_that_name_each_other_are_rejected() {
    let e = env("type A = B\ntype B = A");
    assert!(
        kinds(&e).iter().any(|k| matches!(k, TypeErrorKind::RecursiveAlias(_))),
        "{:?}",
        errors(&e)
    );
}

#[test]
fn declaration_order_does_not_matter() {
    let mut e = env("type Wrapper = Inner\nrecord Inner { x: i64 }");
    let w = ty(&mut e, "Wrapper");
    let i = ty(&mut e, "Inner");
    assert_clean(&e);
    assert_eq!(w, i);
}

// ---- records ----

#[test]
fn a_nominal_record_satisfies_a_structural_type() {
    let mut e = env("record Person { name: str, age: i64 }");
    let p = ty(&mut e, "Person");
    let has_name = ty(&mut e, "{ name: str }");
    assert_clean(&e);
    assert!(e.solver.is_subtype(p, has_name));
    assert!(!e.solver.is_subtype(has_name, p));
}

#[test]
fn distinct_records_stay_disjoint_despite_a_shared_shape() {
    let mut e = env("record Red { }\nrecord Green { }\ntype Color = Red | Green");
    let r = ty(&mut e, "Red");
    let g = ty(&mut e, "Green");
    let c = ty(&mut e, "Color");
    assert_clean(&e);

    let meet = e.solver.t.intersect(r, g);
    assert!(e.solver.is_empty(meet), "unit records stay nominal");
    assert!(e.solver.is_subtype(r, c));
    let rest = e.solver.t.diff(c, r);
    assert!(e.solver.is_equiv(rest, g), "the union is a sum type");
}

#[test]
fn a_recursive_record_needs_no_binder() {
    let mut e = env("record Node { next: Node | null }");
    let n = ty(&mut e, "Node");
    assert_clean(&e);
    assert!(!e.solver.is_empty(n), "the `null` arm is the base case");

    // Equi-recursive: `next` holds the record again, with no wrapper. If the
    // recursion were lost, `next` would be `null` alone and this would hold.
    let terminal = ty(&mut e, "{ next: null }");
    assert!(!e.solver.is_subtype(n, terminal), "a Node's `next` is a Node or null");
}

#[test]
fn a_nominal_recursive_record_satisfies_a_structural_mu_type() {
    let mut e = env("record Node { next: Node | null }\nmu type T = { next: T | null }");
    assert_clean(&e);
    let n = ty(&mut e, "Node");
    let t = ty(&mut e, "T");
    assert_clean(&e);
    assert!(e.solver.is_subtype(n, t), "a structural mu accepts a family of records");

    let terminal = ty(&mut e, "{ next: null }");
    assert!(!e.solver.is_subtype(t, terminal), "T's `next` is a T or null");
}

#[test]
fn a_duplicate_field_is_rejected() {
    let e = env("record Bad { x: i64, x: str }");
    assert_eq!(kinds(&e), vec![TypeErrorKind::DuplicateField("x".into())]);
}

#[test]
fn a_duplicate_declaration_is_rejected() {
    let e = env("record R { x: i64 }\ntype R = str");
    assert_eq!(kinds(&e), vec![TypeErrorKind::Duplicate("R".into())]);
}

// ---- generics ----

#[test]
fn generic_arguments_are_covariant() {
    let mut e = env(
        "record Circle { radius: i64 }\n\
         record Square { side: i64 }\n\
         type Shape = Circle | Square\n\
         record Box[T] { item: T }",
    );
    let bc = ty(&mut e, "Box[Circle]");
    let bs = ty(&mut e, "Box[Shape]");
    assert_clean(&e);

    assert!(e.solver.is_subtype(bc, bs), "Box[Circle] <: Box[Shape]");
    assert!(!e.solver.is_subtype(bs, bc));
}

#[test]
fn distinct_instantiations_are_disjoint() {
    let mut e = env("record Box[T] { item: T }");
    let bi = ty(&mut e, "Box[i64]");
    let bs = ty(&mut e, "Box[str]");
    assert_clean(&e);
    let meet = e.solver.t.intersect(bi, bs);
    assert!(e.solver.is_empty(meet));
}

#[test]
fn a_generic_field_is_substituted_not_left_rigid() {
    let mut e = env("record Box[T] { item: T }");
    let b = ty(&mut e, "Box[i64]");
    let wants = ty(&mut e, "{ item: i64 }");
    assert_clean(&e);
    assert!(e.solver.is_subtype(b, wants));
}

#[test]
fn wrong_generic_arity_is_rejected() {
    let mut e = env("record Box[T] { item: T }");
    let t = ty(&mut e, "Box[i64, str]");
    assert!(e.is_error(t));
    assert_eq!(
        kinds(&e),
        vec![TypeErrorKind::Arity { name: "Box".into(), expected: 1, found: 2 }]
    );
}

#[test]
fn a_type_parameter_is_rigid_in_its_own_signature() {
    let mut e = env("record Box[T] { item: T }\nfn unwrap[T](b: Box[T]) -> T { b.item }");
    assert_clean(&e);
    let sig = e.fns().iter().find(|f| f.name == "unwrap").expect("declared").clone();
    assert_eq!(sig.generics, vec!["T".to_string()]);

    let i = e.solver.t.i64();
    assert!(!e.solver.is_subtype(i, sig.ret), "T is opaque, not i64");
}

// ---- newtypes ----

#[test]
fn a_newtype_is_distinct_from_its_representation_and_its_siblings() {
    let mut e = env("newtype Meter = f64\nnewtype Second = f64");
    let m = ty(&mut e, "Meter");
    let s = ty(&mut e, "Second");
    let f = ty(&mut e, "f64");
    assert_clean(&e);

    assert!(!e.solver.is_subtype(f, m), "the representation does not flow in");
    assert!(!e.solver.is_subtype(m, f), "and the newtype does not flow out");
    let meet = e.solver.t.intersect(m, s);
    assert!(e.solver.is_empty(meet), "two newtypes over f64 are still different types");
}

#[test]
fn a_recursive_newtype_is_rejected() {
    let e = env("record Box[T] { item: T }\nnewtype W = Box[W]");
    assert!(
        kinds(&e).contains(&TypeErrorKind::RecursiveNewtype("W".into())),
        "{:?}",
        errors(&e)
    );
}

// ---- mu ----

fn mu_env(decl: &str) -> Env {
    env(&format!("record Box[T] {{ item: T }}\n{decl}"))
}

#[test]
fn a_mu_type_resolves_through_a_generic_argument() {
    let mut e = mu_env("mu type A = :ok | Box[A]");
    assert_clean(&e);
    let a = ty(&mut e, "A");
    assert_clean(&e);

    assert!(!e.solver.is_empty(a), "`:ok` is the base case");
    assert!(e.solver.is_subtype(a, a));

    let ok = ty(&mut e, ":ok");
    assert!(e.solver.is_subtype(ok, a));
    let boxed_ok = ty(&mut e, "Box[:ok]");
    assert!(e.solver.is_subtype(boxed_ok, a), "Box[:ok] <: A");
}

#[test]
fn a_mu_type_is_equi_recursive() {
    let mut e = mu_env("mu type A = :ok | Box[A]");
    let a = ty(&mut e, "A");
    let unfolded = ty(&mut e, ":ok | Box[A]");
    assert_clean(&e);
    assert!(e.solver.is_equiv(a, unfolded), "A and its unfolding are one type");
}

#[test]
fn a_mu_type_may_recurse_through_several_constructors_at_once() {
    let mut e = env(
        "record List[T] { head: T | null }\n\
         record Map[K, V] { key: K, value: V }\n\
         mu type Json = null | bool | f64 | str | List[Json] | Map[str, Json]",
    );
    assert_clean(&e);
    let j = ty(&mut e, "Json");
    assert_clean(&e);
    assert!(!e.solver.is_empty(j));

    let f = ty(&mut e, "f64");
    assert!(e.solver.is_subtype(f, j));
    let nested = ty(&mut e, "List[List[f64]]");
    assert!(e.solver.is_subtype(nested, j), "the recursion has no fixed depth");
}

#[test]
fn a_mu_type_guarded_by_a_visible_record_field_is_contractive() {
    let e = env("record Node { next: T | null }\nmu type T = Node");
    assert_clean(&e);
}

#[test]
fn an_unguarded_recursive_occurrence_is_rejected() {
    let e = env("mu type T = T | i64");
    assert_eq!(kinds(&e), vec![TypeErrorKind::MuUnguarded("T".into())]);
    let d = &errors(&e)[0];
    assert!(d.contains("T") && d.contains("guarded"));
}

#[test]
fn recursion_beneath_a_negation_is_rejected() {
    let e = mu_env("mu type T = i64 | Box[!T]");
    assert_eq!(kinds(&e), vec![TypeErrorKind::MuUnderNegation("T".into())]);
    assert!(errors(&e)[0].contains("negation"));
}

#[test]
fn recursion_through_an_arrow_return_is_allowed() {
    // A return is covariant and the arrow guards it, so this is contractive.
    let e = env("mu type F = null | (i64) -> F");
    assert_eq!(kinds(&e), vec![]);
}

#[test]
fn recursion_in_a_parameter_is_rejected() {
    let e = env("mu type F = null | (F) -> i64");
    assert_eq!(kinds(&e), vec![TypeErrorKind::MuInParameter("F".into())]);
    assert!(errors(&e)[0].contains("contravariant"));
}

#[test]
fn a_parameter_is_rejected_even_when_a_return_also_recurses() {
    let e = env("mu type F = null | (F) -> F");
    assert_eq!(kinds(&e), vec![TypeErrorKind::MuInParameter("F".into())]);
}

#[test]
fn a_mu_type_that_never_names_itself_is_rejected() {
    let e = env("mu type NotRecursive = i64 | str");
    assert_eq!(kinds(&e), vec![TypeErrorKind::MuWithoutRecursion("NotRecursive".into())]);
    assert!(errors(&e)[0].contains("mu"));
}

#[test]
fn mutually_recursive_mu_types_are_a_clear_error() {
    let e = mu_env("mu type Even = null | Box[Odd]\nmu type Odd = null | Box[Even]");
    let ds = errors(&e);
    assert!(!ds.is_empty(), "mutual recursion is rejected, not resolved");
    assert!(ds.iter().all(|d| d.contains("mutual")), "{ds:?}");
}

#[test]
fn a_rejected_mu_poisons_its_uses_without_a_second_complaint() {
    let mut e = env("mu type T = T | i64");
    let before = e.errors().len();
    let t = ty(&mut e, "T");
    assert!(e.is_error(t));
    assert_eq!(e.errors().len(), before, "one bad declaration, one diagnostic");
}

#[test]
fn a_tuple_element_guards_recursion() {
    let e = env("mu type L = :nil | (i64, L)");
    assert_clean(&e);
}

#[test]
fn a_structural_field_guards_recursion() {
    let e = env("mu type T = null | { next: T }");
    assert_clean(&e);
}

#[test]
fn a_generic_mu_type_instantiates() {
    let mut e = mu_env("mu type Tree[T] = T | Box[Tree[T]]");
    assert_clean(&e);
    let t = ty(&mut e, "Tree[i64]");
    assert_clean(&e);

    let i = ty(&mut e, "i64");
    assert!(e.solver.is_subtype(i, t));
    let nested = ty(&mut e, "Box[Box[i64]]");
    assert!(e.solver.is_subtype(nested, t));
}

// ---- opaque is module-scoped ----

#[test]
fn an_opaque_record_expands_inside_its_own_module() {
    // `Rng` is a data constructor with a guardable field where its fields are
    // visible, so the same `mu type` is well-formed here...
    let e = env("mod rand { opaque record Rng { seed: T | null }\n mu type T = Rng }");
    assert_clean(&e);
}

#[test]
fn an_opaque_records_fields_reach_one_parent_module() {
    let e = env("mod rand { opaque record Rng { seed: T | null } }\nmu type T = rand::Rng");
    assert_clean(&e);
}

#[test]
fn an_opaque_record_is_an_atom_beyond_its_parent() {
    // ...and an atom out here, with no position for the recursion to sit in.
    let e = env(
        "mod std { mod rand { opaque record Rng { seed: T | null } } }\n\
         mu type T = std::rand::Rng",
    );
    assert!(
        kinds(&e).contains(&TypeErrorKind::MuWithoutRecursion("T".into())),
        "{:?}",
        errors(&e)
    );
}

#[test]
fn a_transparent_record_expands_from_anywhere() {
    let e = env("mod m { record Node { next: T | null } }\nmu type T = m::Node");
    assert_clean(&e);
}

// ---- modules ----

#[test]
fn a_qualified_path_and_a_use_reach_the_same_declaration() {
    let mut e = env("mod m { record Point { x: i64 } }\nuse m::Point");
    let a = ty(&mut e, "m::Point");
    let b = ty(&mut e, "Point");
    assert_clean(&e);
    assert_eq!(a, b);
}

// ---- what dispatch consumes ----

#[test]
fn protocols_and_impls_are_registered_with_resolved_signatures() {
    let mut e = env(
        "record Circle { radius: i64 }\n\
         protocol Area for T { fn area(t: T) -> i64 }\n\
         impl Area for Circle { fn area(t: Circle) -> i64 { t.radius } }",
    );
    assert_clean(&e);

    let ps = e.protocols_with_method("area");
    assert_eq!(ps.len(), 1);
    let p = e.protocol(ps[0]);
    assert_eq!(p.name, "Area");
    assert_eq!(p.subject, "T");
    assert_eq!(p.subject_arity, 0);

    let targets: Vec<_> = e.impls_of(ps[0]).map(|(_, i)| i.target).collect();
    assert_eq!(targets.len(), 1);
    let target = targets[0].expect("a concrete target");
    let circle = ty(&mut e, "Circle");
    assert_eq!(target, circle);
}

#[test]
fn a_constructor_impl_records_the_head_rather_than_a_type() {
    let e = env(
        "record Box[T] { item: T }\n\
         protocol Container for C[_] { fn size[T](c: C[T]) -> i64 }\n\
         impl Container for Box { fn size[T](c: Box[T]) -> i64 { 1 } }",
    );
    assert_clean(&e);
    let i = &e.impls()[0];
    assert_eq!(i.target_head.as_deref(), Some("Box"));
    assert!(i.target.is_none(), "`Box` names a constructor, not a type");
}

#[test]
fn an_impl_of_an_unknown_protocol_is_rejected() {
    let e = env("record Circle { radius: i64 }\nimpl Nope for Circle { }");
    assert_eq!(kinds(&e), vec![TypeErrorKind::UnknownProtocol("Nope".into())]);
}

#[test]
fn a_fn_signature_carries_its_throws_in_its_arrow_and_its_bounds_apart() {
    let mut e = env(
        "record IoError { }\n\
         protocol Display for T { fn show(t: T) -> str }\n\
         fn dump[T](x: T) throws IoError -> str where T: Display { \"\" }",
    );
    assert_clean(&e);
    let sig = e.fns().iter().find(|f| f.name == "dump").expect("declared").clone();

    let nothrow = e.solver.t.never();
    assert_ne!(sig.throws, nothrow, "`throws IoError` was resolved");
    let ps = sig.params.iter().map(|p| p.1).collect();
    let erased = e.solver.t.arrow(ps, nothrow, sig.ret);
    assert_ne!(sig.ty, erased, "`throws` is part of the arrow");

    // A bound is a protocol path, not a type: it has nowhere to live in the arrow.
    assert_eq!(sig.wheres, vec![("T".to_string(), vec!["Display".to_string()])]);
    assert!(sig.has_body);
}

#[test]
fn a_fn_with_no_return_type_returns_unit() {
    let mut e = env("fn go() { }");
    assert_clean(&e);
    let unit = e.solver.t.tuple(vec![]);
    let sig = e.fns().iter().find(|f| f.name == "go").expect("declared").clone();
    assert_eq!(sig.ret, unit);
}

#[test]
fn a_protocol_method_has_no_body() {
    let e = env("protocol Area for T { fn area(t: T) -> i64 }");
    assert_clean(&e);
    assert!(!e.protocols()[0].methods[0].has_body);
}


// ---- orphan impls ----

fn env_as(src: &str, unit: super::env::Unit) -> Env {
    Env::build_as(&parse(src), unit)
}

const SHAPES: &str = "
    record Circle { r: i64 }
    record Square { s: i64 }
    record Tri { t: i64 }
    protocol Area for T { fn area(v: T) -> i64 }
";

#[test]
fn an_orphan_that_fills_a_gap_is_accepted() {
    let e = env(&format!(
        "{SHAPES}
         impl Area for Circle {{ fn area(v: Circle) -> i64 {{ 1 }} }}
         orphan impl Area for Square {{ fn area(v: Square) -> i64 {{ 2 }} }}"
    ));
    assert_eq!(kinds(&e), vec![], "Square is disjoint from Circle, so nothing is stolen");
}

#[test]
fn an_orphan_may_not_steal_covered_values() {
    let e = env(&format!(
        "{SHAPES}
         impl Area for Circle {{ fn area(v: Circle) -> i64 {{ 1 }} }}
         orphan impl Area for Circle {{ fn area(v: Circle) -> i64 {{ 2 }} }}"
    ));
    assert!(matches!(kinds(&e).as_slice(), [TypeErrorKind::OrphanOverlaps { .. }]));
    assert!(errors(&e)[0].contains("gap"));
    // The intersection IS the diagnostic: it names the values, not just the protocol.
    assert!(errors(&e)[0].contains("Circle"), "{}", errors(&e)[0]);
}

#[test]
fn an_orphan_may_not_specialize_a_wider_impl() {
    // The hijack decisions.md names: the root quietly taking Circle values out of a
    // library's `impl Area for Shape`, so the library's own code stops taking its
    // own path. Circle <: Shape, so this is an overlap, not a gap.
    let e = env(&format!(
        "{SHAPES}
         type Shape = Circle | Square
         impl Area for Shape {{ fn area(v: Shape) -> i64 {{ 1 }} }}
         orphan impl Area for Circle {{ fn area(v: Circle) -> i64 {{ 2 }} }}"
    ));
    assert!(matches!(kinds(&e).as_slice(), [TypeErrorKind::OrphanOverlaps { .. }]));
    assert!(errors(&e)[0].contains("Circle"), "the stolen values are named: {}", errors(&e)[0]);
}

#[test]
fn an_orphan_gap_beside_a_wider_impl_is_still_a_gap() {
    let e = env(&format!(
        "{SHAPES}
         type Shape = Circle | Square
         impl Area for Shape {{ fn area(v: Shape) -> i64 {{ 1 }} }}
         orphan impl Area for Tri {{ fn area(v: Tri) -> i64 {{ 2 }} }}"
    ));
    assert_eq!(kinds(&e), vec![], "Tri is in neither arm of Shape");
}

#[test]
fn a_library_may_not_carry_an_orphan() {
    let e = env_as(
        &format!(
            "{SHAPES}
             orphan impl Area for Circle {{ fn area(v: Circle) -> i64 {{ 1 }} }}"
        ),
        super::env::Unit::Library,
    );
    assert_eq!(kinds(&e), vec![TypeErrorKind::OrphanInLibrary("Area".into())]);
    assert!(errors(&e)[0].contains("root application"));
}

#[test]
fn the_root_application_may_carry_the_same_orphan() {
    let e = env_as(
        &format!(
            "{SHAPES}
             orphan impl Area for Circle {{ fn area(v: Circle) -> i64 {{ 1 }} }}"
        ),
        super::env::Unit::RootApplication,
    );
    assert_eq!(kinds(&e), vec![], "exactly one root, so it cannot disagree with itself");
}

#[test]
fn a_plain_impl_is_not_subject_to_the_gap_rule() {
    // Nested overlap is legal for whoever owns a side: Circle's impl wins for Circle
    // values. Only orphans are restricted to filling gaps.
    let e = env(&format!(
        "{SHAPES}
         type Shape = Circle | Square
         impl Area for Shape {{ fn area(v: Shape) -> i64 {{ 1 }} }}
         impl Area for Circle {{ fn area(v: Circle) -> i64 {{ 2 }} }}"
    ));
    assert_eq!(kinds(&e), vec![]);
}

// ---- names reach the printer ----

#[test]
fn a_mu_type_prints_by_name_rather_than_as_a_binder() {
    // `mu A0 = ...` is not syntax anyone can write, so a recursive type with a name
    // must reach for it. `defs` is what carries the name from env to the printer,
    // and nothing populated it until now.
    let mut e = env("record Wrap { v: Json | null }\nmu type Json = Wrap");
    let json = ty(&mut e, "Json");
    let shown = super::print::print(&mut e.solver.t, json);
    assert_eq!(shown, "Json");
}

#[test]
fn a_generic_alias_is_not_recorded_under_its_bare_name() {
    // `Pair[i64]` and `Pair[str]` would collide on `Pair`, and printing one as the
    // other is worse than printing the expansion.
    let mut e = env("type Pair[T] = (T, T)");
    let pi = ty(&mut e, "Pair[i64]");
    let ps = ty(&mut e, "Pair[str]");
    let a = super::print::print(&mut e.solver.t, pi);
    let b = super::print::print(&mut e.solver.t, ps);
    assert_ne!(a, b, "two instantiations must not print alike: {a} vs {b}");
    assert!(!a.contains("Pair"), "no name is better than the wrong name: {a}");
}

// ---- multi-module build ----

#[test]
fn a_program_resolves_a_name_from_another_module() {
    // The stdlib-loading mechanism, without a filesystem: an `io` module declared
    // under the prefix `std::io`, and a user program that calls into it.
    let io = parse("@native(\"neon_io_println\") fn println(s: str)");
    let user = parse("use std::io\nfn main() { io::println(\"hi\") }");
    let env = Env::build_with(
        &[(vec!["std".into(), "io".into()], &io), (vec![], &user)],
        super::env::Unit::RootApplication,
    );
    assert!(env.errors().is_empty(), "{:?}", env.errors());

    // The real proof: the body resolves `io::println` through the `use` alias.
    let mut env = env;
    let (_r, errs) = super::check::check_module(&mut env, &user);
    assert!(errs.is_empty(), "io::println did not resolve: {errs:?}");
}

#[test]
fn a_stdlib_fn_may_reference_a_later_module() {
    // All modules are declared before any body resolves, so order does not matter:
    // the second module's type is visible to the first.
    let a = parse("fn wrap(p: b::Point) -> i64 { p.x }");
    let b = parse("record Point { x: i64 }");
    let env = Env::build_with(
        &[(vec!["a".into()], &a), (vec!["b".into()], &b)],
        super::env::Unit::RootApplication,
    );
    assert!(env.errors().is_empty(), "{:?}", env.errors());
}

// ---- use trees ----

fn build2(prefix: &[&str], lib: &str, user: &str) -> Env {
    let lib = parse(lib);
    let user = parse(user);
    let p: Vec<String> = prefix.iter().map(|s| s.to_string()).collect();
    let mut env = Env::build_with(
        &[(p, &lib), (vec![], &user)],
        super::env::Unit::RootApplication,
    );
    let (_r, errs) = super::check::check_module(&mut env, &user);
    env.extend_errors(errs);
    env
}

#[test]
fn a_glob_use_brings_a_module_member_into_scope() {
    let e = build2(
        &["std", "io"],
        "@native(\"p\") fn println(s: str)",
        "use std::io::*\nfn main() { println(\"hi\") }",
    );
    assert!(e.errors().is_empty(), "{:?}", e.errors());
}

#[test]
fn a_renamed_use_binds_the_alias_not_the_last_segment() {
    let e = build2(
        &["std", "io"],
        "@native(\"p\") fn println(s: str)",
        "use std::io::println as say\nfn main() { say(\"hi\") }",
    );
    assert!(e.errors().is_empty(), "{:?}", e.errors());
}

#[test]
fn a_grouped_use_imports_each_member() {
    let e = build2(
        &["std", "io"],
        "@native(\"p\") fn println(s: str)\n@native(\"e\") fn eprintln(s: str)",
        "use std::io::{println, eprintln}\nfn main() { println(\"a\"); eprintln(\"b\") }",
    );
    assert!(e.errors().is_empty(), "{:?}", e.errors());
}

#[test]
fn an_explicit_binding_beats_a_glob() {
    // Two modules both export `f`; the explicit import wins over the glob.
    let a = parse("@native(\"af\") fn f() -> i64");
    let b = parse("@native(\"bf\") fn f() -> str");
    let user = parse("use a::*\nuse b::f\nfn main() -> str { f() }");
    let mut env = Env::build_with(
        &[(vec!["a".into()], &a), (vec!["b".into()], &b), (vec![], &user)],
        super::env::Unit::RootApplication,
    );
    let (_r, errs) = super::check::check_module(&mut env, &user);
    // b::f returns str, matching the annotation. If the glob's a::f (i64) had won,
    // this would be a mismatch.
    assert!(errs.is_empty(), "explicit import should win: {errs:?}");
}

#[test]
fn importing_a_protocol_method_disambiguates_the_call() {
    // Two protocols declare `go`, which is normally ambiguous. Importing A's method
    // by path picks A without a qualifier at the call site.
    let m = parse(
        "record R { x: i64 }
         protocol A for T { fn go(v: T) -> str }
         protocol B for T { fn go(v: T) -> str }
         impl A for R { fn go(v: R) -> str { \"a\" } }
         impl B for R { fn go(v: R) -> str { \"b\" } }
         use A::go
         fn f(r: R) -> str { go(r) }",
    );
    let mut env = Env::build(&m);
    let (_r, errs) = super::check::check_module(&mut env, &m);
    assert!(errs.is_empty(), "the import should disambiguate: {errs:?}");
}

#[test]
fn without_the_import_the_ambiguous_call_still_errors() {
    let m = parse(
        "record R { x: i64 }
         protocol A for T { fn go(v: T) -> str }
         protocol B for T { fn go(v: T) -> str }
         impl A for R { fn go(v: R) -> str { \"a\" } }
         impl B for R { fn go(v: R) -> str { \"b\" } }
         fn f(r: R) -> str { go(r) }",
    );
    let mut env = Env::build(&m);
    let (_r, errs) = super::check::check_module(&mut env, &m);
    assert!(
        errs.iter().any(|e| matches!(e.kind, TypeErrorKind::AmbiguousCall { .. })),
        "{errs:?}"
    );
}
