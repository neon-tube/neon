use super::empty::Solver;
use super::types::*;

fn s() -> Solver {
    Solver::new()
}

// ---- the lattice ----

#[test]
fn never_is_empty_and_any_is_not() {
    let mut s = s();
    let n = s.t.never();
    let a = s.t.any();
    assert!(s.is_empty(n));
    assert!(!s.is_empty(a));
}

#[test]
fn never_is_below_everything_and_any_above() {
    let mut s = s();
    let n = s.t.never();
    let a = s.t.any();
    for t in [s.t.i64(), s.t.str(), s.t.bool(), s.t.null(), a, n] {
        assert!(s.is_subtype(n, t));
        assert!(s.is_subtype(t, a));
    }
}

#[test]
fn reflexive() {
    let mut s = s();
    let ok = s.t.name("ok");
    let atom = s.t.atom(ok);
    for t in [s.t.i64(), s.t.str(), atom, s.t.any(), s.t.never()] {
        assert!(s.is_subtype(t, t));
    }
}

#[test]
fn distinct_primitives_are_disjoint() {
    let mut s = s();
    let i = s.t.i64();
    let st = s.t.str();
    assert!(!s.is_subtype(i, st));
    assert!(!s.is_subtype(st, i));
    let both = s.t.intersect(i, st);
    assert!(s.is_empty(both));
}

#[test]
fn union_absorbs_and_intersect_narrows() {
    let mut s = s();
    let i = s.t.i64();
    let st = s.t.str();
    let u = s.t.union(i, st);
    assert!(s.is_subtype(i, u));
    assert!(s.is_subtype(st, u));
    assert!(!s.is_subtype(u, i));
}

#[test]
fn difference_recovers_the_other_arm() {
    let mut s = s();
    let i = s.t.i64();
    let st = s.t.str();
    let u = s.t.union(i, st);
    let d = s.t.diff(u, st);
    assert!(s.is_equiv(d, i));
}

#[test]
fn complement_laws() {
    let mut s = s();
    let i = s.t.i64();
    let ni = s.t.negate(i);
    let meet = s.t.intersect(i, ni);
    assert!(s.is_empty(meet));

    // Within the value lattice, `i64 | !i64` is everything.
    let join = s.t.union(i, ni);
    let a = s.t.any();
    assert!(s.is_subtype(a, join));
}

#[test]
fn de_morgan_over_descriptors() {
    let mut s = s();
    let i = s.t.i64();
    let st = s.t.str();
    let lhs = {
        let u = s.t.union(i, st);
        s.t.negate(u)
    };
    let rhs = {
        let a = s.t.negate(i);
        let b = s.t.negate(st);
        s.t.intersect(a, b)
    };
    assert!(s.is_equiv(lhs, rhs));
}

#[test]
fn transitivity() {
    let mut s = s();
    let i = s.t.i64();
    let st = s.t.str();
    let b = s.t.bool();
    let ab = s.t.union(i, st);
    let abc = s.t.union(ab, b);
    assert!(s.is_subtype(i, ab));
    assert!(s.is_subtype(ab, abc));
    assert!(s.is_subtype(i, abc));
}

#[test]
fn hash_consing_makes_equal_types_identical() {
    let mut s = s();
    let i = s.t.i64();
    let st = s.t.str();
    let a = s.t.union(i, st);
    let b = s.t.union(st, i);
    assert_eq!(a, b, "union is commutative and interned");
}

// ---- atoms ----

#[test]
fn atom_singletons() {
    let mut s = s();
    let ok = s.t.name("ok");
    let err = s.t.name("err");
    let a_ok = s.t.atom(ok);
    let a_err = s.t.atom(err);
    let u = s.t.union(a_ok, a_err);

    assert!(s.is_subtype(a_ok, u));
    assert!(!s.is_subtype(u, a_ok));
    let meet = s.t.intersect(a_ok, a_err);
    assert!(s.is_empty(meet), ":ok and :err are distinct singletons");
}

#[test]
fn atoms_are_disjoint_from_primitives() {
    let mut s = s();
    let ok = s.t.name("ok");
    let a_ok = s.t.atom(ok);
    let i = s.t.i64();
    let meet = s.t.intersect(a_ok, i);
    assert!(s.is_empty(meet));
}

#[test]
fn cofinite_atom_sets() {
    let mut s = s();
    let ok = s.t.name("ok");
    let err = s.t.name("err");
    let a_ok = s.t.atom(ok);
    let a_err = s.t.atom(err);

    // `!:ok` is every atom but `:ok` — plus every non-atom. It must contain `:err`.
    let not_ok = s.t.negate(a_ok);
    assert!(s.is_subtype(a_err, not_ok));
    assert!(!s.is_subtype(a_ok, not_ok));
}

#[test]
fn exhaustiveness_falls_out_of_emptiness() {
    let mut s = s();
    let ok = s.t.name("ok");
    let err = s.t.name("err");
    let a_ok = s.t.atom(ok);
    let a_err = s.t.atom(err);
    let subject = s.t.union(a_ok, a_err);

    // Covering both arms leaves nothing.
    let covered = s.t.union(a_ok, a_err);
    let rest = s.t.diff(subject, covered);
    assert!(s.is_empty(rest), "match is exhaustive");

    // Covering one leaves the other.
    let rest = s.t.diff(subject, a_ok);
    assert!(!s.is_empty(rest));
    assert!(s.is_equiv(rest, a_err), "the diagnostic can name `:err` exactly");
}

// ---- records ----

fn nominal(s: &mut Solver, name: &str, fields: &[(&str, TyId)]) -> TyId {
    let n = s.t.name(name);
    let fs = fields
        .iter()
        .map(|(l, t)| {
            let l = s.t.name(l);
            (l, *t)
        })
        .collect();
    s.t.nominal(n, vec![], fs)
}

fn structural(s: &mut Solver, fields: &[(&str, TyId)]) -> TyId {
    let fs = fields
        .iter()
        .map(|(l, t)| {
            let l = s.t.name(l);
            (l, *t)
        })
        .collect();
    s.t.struct_ty(fs)
}

#[test]
fn a_record_is_inhabited() {
    let mut s = s();
    let i = s.t.i64();
    let r = nominal(&mut s, "Red", &[("x", i)]);
    assert!(!s.is_empty(r));
}

#[test]
fn distinct_nominals_are_disjoint_despite_identical_shape() {
    let mut s = s();
    let i = s.t.i64();
    let red = nominal(&mut s, "Red", &[("x", i)]);
    let green = nominal(&mut s, "Green", &[("x", i)]);
    let meet = s.t.intersect(red, green);
    assert!(s.is_empty(meet));
    assert!(!s.is_subtype(red, green));
}

#[test]
fn nominal_satisfies_structural() {
    let mut s = s();
    let i = s.t.i64();
    let st = s.t.str();
    let person = nominal(&mut s, "Person", &[("name", st), ("age", i)]);
    let has_name = structural(&mut s, &[("name", st)]);

    assert!(s.is_subtype(person, has_name), "width subtyping: Person has a name");
    assert!(
        !s.is_subtype(has_name, person),
        "but an anonymous record with a name is not a Person"
    );
}

#[test]
fn structural_field_type_must_match() {
    let mut s = s();
    let i = s.t.i64();
    let st = s.t.str();
    let person = nominal(&mut s, "Person", &[("name", st)]);
    let wants_i64_name = structural(&mut s, &[("name", i)]);
    assert!(!s.is_subtype(person, wants_i64_name));
}

#[test]
fn missing_field_is_not_satisfied() {
    let mut s = s();
    let st = s.t.str();
    let person = nominal(&mut s, "Person", &[("name", st)]);
    let wants_email = structural(&mut s, &[("email", st)]);
    assert!(!s.is_subtype(person, wants_email), "Person has no email");
}

#[test]
fn structural_is_open() {
    let mut s = s();
    let i = s.t.i64();
    let st = s.t.str();
    let wide = structural(&mut s, &[("name", st), ("age", i)]);
    let narrow = structural(&mut s, &[("name", st)]);
    assert!(s.is_subtype(wide, narrow));
    assert!(!s.is_subtype(narrow, wide));
}

// ---- generics ----

fn boxed(s: &mut Solver, arg: TyId) -> TyId {
    let n = s.t.name("Box");
    s.t.nominal(n, vec![arg], vec![])
}

#[test]
fn generic_arguments_are_covariant() {
    let mut s = s();
    let i = s.t.i64();
    let st = s.t.str();
    let u = s.t.union(i, st);

    let box_i = boxed(&mut s, i);
    let box_u = boxed(&mut s, u);

    assert!(s.is_subtype(box_i, box_u), "Box[i64] <: Box[i64|str]");
    assert!(!s.is_subtype(box_u, box_i));
}

#[test]
fn distinct_generic_arguments_are_disjoint() {
    let mut s = s();
    let i = s.t.i64();
    let st = s.t.str();
    let box_i = boxed(&mut s, i);
    let box_s = boxed(&mut s, st);
    let meet = s.t.intersect(box_i, box_s);
    assert!(s.is_empty(meet));
}

#[test]
fn covariance_lets_one_instantiation_cover_a_union() {
    let mut s = s();
    let i = s.t.i64();
    let st = s.t.str();
    let box_i = boxed(&mut s, i);
    let box_s = boxed(&mut s, st);
    let either = s.t.union(box_i, box_s);

    let u = s.t.union(i, st);
    let box_u = boxed(&mut s, u);

    // This is what lets `impl[T] Sized for Box[T]` match `Box[i64] | Box[str]` with a
    // single T := i64|str, instead of needing two instantiations and a runtime switch.
    assert!(s.is_subtype(either, box_u));
}

// ---- tuples ----

#[test]
fn tuples_are_covariant_and_arity_separates() {
    let mut s = s();
    let i = s.t.i64();
    let st = s.t.str();
    let u = s.t.union(i, st);

    let t1 = s.t.tuple(vec![i, i]);
    let t2 = s.t.tuple(vec![u, u]);
    assert!(s.is_subtype(t1, t2));
    assert!(!s.is_subtype(t2, t1));

    let t3 = s.t.tuple(vec![i]);
    let meet = s.t.intersect(t1, t3);
    assert!(s.is_empty(meet), "different arity, disjoint");
}

#[test]
fn tuple_difference() {
    let mut s = s();
    let i = s.t.i64();
    let st = s.t.str();
    let u = s.t.union(i, st);
    let wide = s.t.tuple(vec![u]);
    let narrow = s.t.tuple(vec![i]);
    let d = s.t.diff(wide, narrow);
    let expect = s.t.tuple(vec![st]);
    assert!(s.is_equiv(d, expect));
}

// ---- arrows ----

#[test]
fn arrows_are_contravariant_in_parameters() {
    let mut s = s();
    let i = s.t.i64();
    let st = s.t.str();
    let u = s.t.union(i, st);

    let accepts_more = s.t.arrow(vec![u], i);
    let accepts_less = s.t.arrow(vec![i], i);

    assert!(
        s.is_subtype(accepts_more, accepts_less),
        "(i64|str) -> i64  <:  (i64) -> i64"
    );
    assert!(!s.is_subtype(accepts_less, accepts_more));
}

#[test]
fn arrows_are_covariant_in_return() {
    let mut s = s();
    let i = s.t.i64();
    let st = s.t.str();
    let u = s.t.union(i, st);

    let narrow_ret = s.t.arrow(vec![i], i);
    let wide_ret = s.t.arrow(vec![i], u);
    assert!(s.is_subtype(narrow_ret, wide_ret));
    assert!(!s.is_subtype(wide_ret, narrow_ret));
}

#[test]
fn arrow_arity_separates() {
    let mut s = s();
    let i = s.t.i64();
    let a1 = s.t.arrow(vec![i], i);
    let a2 = s.t.arrow(vec![i, i], i);
    let meet = s.t.intersect(a1, a2);
    assert!(s.is_empty(meet));
}

#[test]
fn arrow_is_disjoint_from_other_kinds() {
    let mut s = s();
    let i = s.t.i64();
    let a = s.t.arrow(vec![i], i);
    let meet = s.t.intersect(a, i);
    assert!(s.is_empty(meet));
}

#[test]
fn arrow_intersection_is_overloading() {
    let mut s = s();
    let i = s.t.i64();
    let st = s.t.str();

    // (i64 -> i64) & (str -> str) is the overloaded function.
    let f = {
        let a = s.t.arrow(vec![i], i);
        let b = s.t.arrow(vec![st], st);
        s.t.intersect(a, b)
    };
    assert!(!s.is_empty(f));

    let ii = s.t.arrow(vec![i], i);
    assert!(s.is_subtype(f, ii), "the overload can be used at i64 -> i64");

    let u = s.t.union(i, st);
    let uu = s.t.arrow(vec![u], u);
    assert!(
        s.is_subtype(f, uu),
        "and at (i64|str) -> (i64|str), which is the point of the FCB rule"
    );
}

// ---- recursion ----

/// `mu A = :ok | Box[A]`
fn mu_ok_or_box(s: &mut Solver) -> TyId {
    let a = s.t.reserve();
    let box_a = boxed(s, a);
    let ok = s.t.name("ok");
    let a_ok = s.t.atom(ok);
    let body = s.t.union(a_ok, box_a);
    let d = s.t.data(body);
    s.t.define(a, d);
    a
}

#[test]
fn recursive_type_is_inhabited() {
    let mut s = s();
    let a = mu_ok_or_box(&mut s);
    assert!(!s.is_empty(a), "`:ok` is the base case");
}

#[test]
fn recursive_type_is_reflexive() {
    let mut s = s();
    let a = mu_ok_or_box(&mut s);
    assert!(s.is_subtype(a, a));
}

#[test]
fn recursive_type_is_equi_recursive() {
    let mut s = s();
    let a = mu_ok_or_box(&mut s);

    // The unfolding `:ok | Box[A]` is the *same type* as `A` — no fold, no wrapper.
    let box_a = boxed(&mut s, a);
    let ok = s.t.name("ok");
    let a_ok = s.t.atom(ok);
    let unfolded = s.t.union(a_ok, box_a);

    assert!(s.is_equiv(a, unfolded));
}

#[test]
fn base_case_is_a_member_of_the_recursive_type() {
    let mut s = s();
    let a = mu_ok_or_box(&mut s);
    let ok = s.t.name("ok");
    let a_ok = s.t.atom(ok);
    assert!(s.is_subtype(a_ok, a));
}

#[test]
fn nested_member_of_the_recursive_type() {
    let mut s = s();
    let a = mu_ok_or_box(&mut s);
    let ok = s.t.name("ok");
    let a_ok = s.t.atom(ok);
    let b = boxed(&mut s, a_ok);
    assert!(s.is_subtype(b, a), "Box[:ok] <: A");
}

#[test]
fn recursion_with_no_base_case_is_empty() {
    let mut s = s();
    // `mu B = Box[B]` — no finite value inhabits it. The coinductive assumption is
    // what decides this: assume B empty, and the derivation finds no contradiction.
    let b = s.t.reserve();
    let box_b = boxed(&mut s, b);
    let d = s.t.data(box_b);
    s.t.define(b, d);
    assert!(s.is_empty(b));
}

#[test]
fn recursive_emptiness_does_not_poison_the_memo() {
    let mut s = s();
    let a = mu_ok_or_box(&mut s);
    // Query twice: a result computed under an assumption must not be cached as if it
    // were unconditional.
    assert!(!s.is_empty(a));
    assert!(!s.is_empty(a));
    assert!(s.is_subtype(a, a));

    let i = s.t.i64();
    assert!(!s.is_empty(i));
    assert!(!s.is_subtype(i, a));
}
