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
    let nothrow = s.t.never();

    let accepts_more = s.t.arrow(vec![u], nothrow, i);
    let accepts_less = s.t.arrow(vec![i], nothrow, i);

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
    let nothrow = s.t.never();

    let narrow_ret = s.t.arrow(vec![i], nothrow, i);
    let wide_ret = s.t.arrow(vec![i], nothrow, u);
    assert!(s.is_subtype(narrow_ret, wide_ret));
    assert!(!s.is_subtype(wide_ret, narrow_ret));
}

#[test]
fn arrow_arity_separates() {
    let mut s = s();
    let nothrow = s.t.never();
    let i = s.t.i64();
    let a1 = s.t.arrow(vec![i], nothrow, i);
    let a2 = s.t.arrow(vec![i, i], nothrow, i);
    let meet = s.t.intersect(a1, a2);
    assert!(s.is_empty(meet));
}

#[test]
fn arrow_is_disjoint_from_other_kinds() {
    let mut s = s();
    let nothrow = s.t.never();
    let i = s.t.i64();
    let a = s.t.arrow(vec![i], nothrow, i);
    let meet = s.t.intersect(a, i);
    assert!(s.is_empty(meet));
}

#[test]
fn arrow_intersection_is_overloading() {
    let mut s = s();
    let i = s.t.i64();
    let st = s.t.str();
    let nothrow = s.t.never();

    // (i64 -> i64) & (str -> str) is the overloaded function.
    let f = {
        let a = s.t.arrow(vec![i], nothrow, i);
        let b = s.t.arrow(vec![st], nothrow, st);
        s.t.intersect(a, b)
    };
    assert!(!s.is_empty(f));

    let ii = s.t.arrow(vec![i], nothrow, i);
    assert!(s.is_subtype(f, ii), "the overload can be used at i64 -> i64");

    let u = s.t.union(i, st);
    let uu = s.t.arrow(vec![u], nothrow, u);
    assert!(
        s.is_subtype(f, uu),
        "and at (i64|str) -> (i64|str), which is the point of the FCB rule"
    );
}

// ---- arrows that throw ----

fn err_atom(s: &mut Solver, name: &str) -> TyId {
    let n = s.t.name(name);
    s.t.atom(n)
}

#[test]
fn arrows_are_covariant_in_throws() {
    let mut s = s();
    let i = s.t.i64();
    let e = err_atom(&mut s, "err");
    let e2 = err_atom(&mut s, "other");
    let both = s.t.union(e, e2);

    let throws_less = s.t.arrow(vec![i], e, i);
    let throws_more = s.t.arrow(vec![i], both, i);
    assert!(
        s.is_subtype(throws_less, throws_more),
        "throwing less is usable where more is expected"
    );
    assert!(!s.is_subtype(throws_more, throws_less));
}

#[test]
fn an_absent_throws_is_never_and_sits_below_every_throws() {
    let mut s = s();
    let i = s.t.i64();
    let nothrow = s.t.never();
    let e = err_atom(&mut s, "err");

    let pure = s.t.arrow(vec![i], nothrow, i);
    let throwing = s.t.arrow(vec![i], e, i);
    assert!(
        s.is_subtype(pure, throwing),
        "(i64) throws never -> i64  <:  (i64) throws :err -> i64"
    );
    assert!(
        !s.is_subtype(throwing, pure),
        "a throwing function cannot stand where a pure one is expected"
    );
}

/// `never` throws is the common case, so a codomain rule that collapses on it
/// would make every same-arity arrow a subtype of every other. Modelling the
/// codomain as `tuple([ret, throws])` does exactly that: the tuple is empty
/// whenever `throws` is `never`, and `never` is below everything, so the return
/// is never compared. The codomain is a sum, not a product.
#[test]
fn a_never_throws_does_not_mask_a_mismatched_return() {
    let mut s = s();
    let i = s.t.i64();
    let st = s.t.str();
    let nothrow = s.t.never();

    let to_i = s.t.arrow(vec![i], nothrow, i);
    let to_str = s.t.arrow(vec![i], nothrow, st);
    assert!(!s.is_subtype(to_i, to_str), "(i64) -> i64 is not (i64) -> str");
    assert!(!s.is_subtype(to_str, to_i));
}

#[test]
fn throws_separates_two_otherwise_identical_arrows() {
    let mut s = s();
    let i = s.t.i64();
    let nothrow = s.t.never();
    let e = err_atom(&mut s, "err");

    let pure = s.t.arrow(vec![i], nothrow, i);
    let throwing = s.t.arrow(vec![i], e, i);
    assert!(!s.is_equiv(pure, throwing), "`throws` is part of the arrow's identity");
}

#[test]
fn an_overload_may_throw_on_one_branch_only() {
    let mut s = s();
    let i = s.t.i64();
    let st = s.t.str();
    let nothrow = s.t.never();
    let e = err_atom(&mut s, "err");

    // (i64 -> i64, pure) & (str throws :err -> str)
    let pure = s.t.arrow(vec![i], nothrow, i);
    let throwing = s.t.arrow(vec![st], e, st);
    let f = s.t.intersect(pure, throwing);
    assert!(!s.is_empty(f), "differing throws do not make the overload empty");

    assert!(s.is_subtype(f, pure), "used at i64 it throws nothing");
    assert!(s.is_subtype(f, throwing));

    // At the merged domain either branch may run, so the merged arrow must admit
    // the throw — and must not claim purity.
    let u = s.t.union(i, st);
    let uu_throwing = s.t.arrow(vec![u], e, u);
    assert!(s.is_subtype(f, uu_throwing));
    let uu_pure = s.t.arrow(vec![u], nothrow, u);
    assert!(
        !s.is_subtype(f, uu_pure),
        "the str branch throws, so the overload is not pure at (i64|str)"
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

// ---- rigid type variables ----

#[test]
fn a_type_variable_is_opaque_and_reflexive() {
    let mut s = s();
    let t = s.t.name("T");
    let v = s.t.var(t);
    assert!(!s.is_empty(v));
    assert!(s.is_subtype(v, v));
}

#[test]
fn distinct_type_variables_are_disjoint() {
    let mut s = s();
    let t = s.t.name("T");
    let u = s.t.name("U");
    let vt = s.t.var(t);
    let vu = s.t.var(u);
    let meet = s.t.intersect(vt, vu);
    assert!(s.is_empty(meet));
    assert!(!s.is_subtype(vt, vu));
}

#[test]
fn a_type_variable_is_disjoint_from_concrete_types() {
    let mut s = s();
    let t = s.t.name("T");
    let v = s.t.var(t);
    let i = s.t.i64();
    let meet = s.t.intersect(v, i);
    assert!(s.is_empty(meet), "T is opaque: it is not i64 until instantiated");
}

#[test]
fn a_type_variable_is_below_any() {
    let mut s = s();
    let t = s.t.name("T");
    let v = s.t.var(t);
    let a = s.t.any();
    assert!(s.is_subtype(v, a), "any is top, so every T fits");
    assert!(!s.is_subtype(a, v));
}

#[test]
fn a_type_variable_survives_negation() {
    let mut s = s();
    let t = s.t.name("T");
    let v = s.t.var(t);
    let nv = s.t.negate(v);
    let meet = s.t.intersect(v, nv);
    assert!(s.is_empty(meet));

    let u = s.t.name("U");
    let vu = s.t.var(u);
    assert!(s.is_subtype(vu, nv), "U is not T, so U <: !T");
}

#[test]
fn a_type_variable_can_be_a_field_and_a_generic_argument() {
    let mut s = s();
    let t = s.t.name("T");
    let v = s.t.var(t);
    let b = boxed(&mut s, v);
    assert!(!s.is_empty(b), "Box[T] is inhabited");

    let i = s.t.i64();
    let bi = boxed(&mut s, i);
    let meet = s.t.intersect(b, bi);
    assert!(s.is_empty(meet), "Box[T] and Box[i64] are disjoint while T is rigid");
}
/// A reserved id read by a boolean operation before it is defined.
///
/// `union` is eager over `TyData`, so it snapshots the reserved id's data — which
/// is `never` until `define`. `record Node { next: Node | null }` therefore
/// resolves to `record Node { next: null }`, silently. Reserve/define only keeps
/// the recursion when the recursive occurrence reaches the body through a raw
/// `TyId` — a field, a generic argument, a tuple element — and never through
/// `union`/`intersect`/`negate`.
#[test]
fn reserve_under_union_keeps_the_recursion() {
    let mut s = s();
    // `record Node { next: Node | null }` built exactly as reserve/define prescribes.
    let n = s.t.reserve();
    let null = s.t.null();
    let next = s.t.union(n, null);
    let label = s.t.name("next");
    let nm = s.t.name("Node");
    let body = s.t.nominal(nm, vec![], vec![(label, next)]);
    let d = s.t.data(body);
    s.t.define(n, d);

    // A one-deep Node must be a Node.
    let inner = s.t.nominal(nm, vec![], vec![(label, null)]);
    let one = {
        let f = s.t.union(inner, null);
        s.t.nominal(nm, vec![], vec![(label, f)])
    };
    assert!(s.is_subtype(one, n), "a one-deep Node is a Node");
}

// ---- recursion through an arrow ----

/// `mu F = null | (i64) -> F`
fn mu_arrow(s: &mut Solver) -> TyId {
    let nothrow = s.t.never();
    let f = s.t.reserve();
    let i = s.t.i64();
    let arrow = s.t.arrow(vec![i], nothrow, f);
    let null = s.t.null();
    let body = s.t.union(null, arrow);
    let d = s.t.data(body);
    s.t.define(f, d);
    f
}

#[test]
fn recursion_through_an_arrow_return_is_inhabited() {
    let mut s = s();
    let f = mu_arrow(&mut s);
    assert!(!s.is_empty(f), "`null` is the base case");
    assert!(s.is_subtype(f, f));
}

#[test]
fn a_one_deep_function_is_a_member() {
    let mut s = s();
    let nothrow = s.t.never();
    let f = mu_arrow(&mut s);
    let i = s.t.i64();
    let null = s.t.null();
    let one = s.t.arrow(vec![i], nothrow, null);
    assert!(s.is_subtype(one, f), "`(i64) -> null` is an F");
}

#[test]
fn a_function_returning_the_wrong_thing_is_not_a_member() {
    let mut s = s();
    let nothrow = s.t.never();
    let f = mu_arrow(&mut s);
    let i = s.t.i64();
    let bad = s.t.arrow(vec![i], nothrow, i);
    assert!(!s.is_subtype(bad, f), "`(i64) -> i64` is not an F");
}

#[test]
fn a_recursive_arrow_is_equi_recursive() {
    let mut s = s();
    let nothrow = s.t.never();
    let f = mu_arrow(&mut s);
    let i = s.t.i64();
    let null = s.t.null();
    let arrow = s.t.arrow(vec![i], nothrow, f);
    let unfolded = s.t.union(null, arrow);
    assert!(s.is_equiv(f, unfolded));
}

/// `mu F = null | (i64) throws :err -> F`
fn mu_throwing_arrow(s: &mut Solver) -> TyId {
    let f = s.t.reserve();
    let i = s.t.i64();
    let e = err_atom(s, "err");
    let arrow = s.t.arrow(vec![i], e, f);
    let null = s.t.null();
    let body = s.t.union(null, arrow);
    let d = s.t.data(body);
    s.t.define(f, d);
    f
}

#[test]
fn recursion_through_a_throwing_arrow_is_inhabited() {
    let mut s = s();
    let f = mu_throwing_arrow(&mut s);
    assert!(!s.is_empty(f), "`null` is the base case");
    assert!(s.is_subtype(f, f));
}

#[test]
fn a_one_deep_throwing_function_is_a_member() {
    let mut s = s();
    let f = mu_throwing_arrow(&mut s);
    let i = s.t.i64();
    let null = s.t.null();
    let e = err_atom(&mut s, "err");
    let one = s.t.arrow(vec![i], e, null);
    assert!(s.is_subtype(one, f), "`(i64) throws :err -> null` is an F");
}

/// The throws is covariant, so a member may throw *less* than the declaration.
#[test]
fn a_pure_function_is_a_member_of_a_throwing_mu() {
    let mut s = s();
    let f = mu_throwing_arrow(&mut s);
    let i = s.t.i64();
    let null = s.t.null();
    let nothrow = s.t.never();
    let pure = s.t.arrow(vec![i], nothrow, null);
    assert!(s.is_subtype(pure, f), "`(i64) -> null` throws less than `:err`");
}

#[test]
fn a_function_throwing_the_wrong_thing_is_not_a_member() {
    let mut s = s();
    let f = mu_throwing_arrow(&mut s);
    let i = s.t.i64();
    let null = s.t.null();
    let other = err_atom(&mut s, "other");
    let bad = s.t.arrow(vec![i], other, null);
    assert!(!s.is_subtype(bad, f), "`throws :other` is not an F");
}

#[test]
fn a_recursive_throwing_arrow_is_equi_recursive() {
    let mut s = s();
    let f = mu_throwing_arrow(&mut s);
    let i = s.t.i64();
    let null = s.t.null();
    let e = err_atom(&mut s, "err");
    let arrow = s.t.arrow(vec![i], e, f);
    let unfolded = s.t.union(null, arrow);
    assert!(s.is_equiv(f, unfolded));
}

#[test]
fn a_recursive_arrow_with_no_base_case_is_empty() {
    let mut s = s();
    let nothrow = s.t.never();
    // `mu G = (i64) -> G`. Unlike the record case this IS inhabited: a function is a
    // value whether or not calling it ever terminates, so the arrow does not need a
    // base case the way a record's field does.
    let g = s.t.reserve();
    let i = s.t.i64();
    let arrow = s.t.arrow(vec![i], nothrow, g);
    let d = s.t.data(arrow);
    s.t.define(g, d);
    assert!(!s.is_empty(g));
}

#[test]
fn a_defined_id_is_the_canonical_one() {
    let mut s = s();
    let a = s.t.reserve();
    let i = s.t.i64();
    let n = s.t.name("Box");
    let body = s.t.nominal(n, vec![i], vec![]);
    let d = s.t.data(body);
    s.t.define(a, d);

    // `define` runs after the body is interned, so an id for this shape already
    // exists. Unless the reserved id is made canonical, `Box[i64]` is one id by
    // name and a different one through any boolean op — and `==`, which is the
    // whole reason to hash-cons, silently answers no.
    let never = s.t.never();
    let through_union = s.t.union(never, a);
    assert_eq!(through_union, a, "`t | never` must be `t`, by id and not merely by meaning");

    let rebuilt = s.t.nominal(n, vec![i], vec![]);
    assert_eq!(rebuilt, a, "rebuilding the shape reaches the defined id");
}

// ---- substitution ----

#[test]
fn substitute_replaces_a_bare_variable() {
    let mut s = s();
    let t = s.t.name("T");
    let v = s.t.var(t);
    let i = s.t.i64();
    let sub = std::collections::HashMap::from([(t, i)]);
    assert_eq!(s.t.substitute(v, &sub), i);
}

#[test]
fn substitute_reaches_into_a_generic_argument() {
    let mut s = s();
    let t = s.t.name("T");
    let v = s.t.var(t);
    let box_t = { let n = s.t.name("Box"); s.t.nominal(n, vec![v], vec![]) };
    let i = s.t.i64();
    let box_i = { let n = s.t.name("Box"); s.t.nominal(n, vec![i], vec![]) };
    let sub = std::collections::HashMap::from([(t, i)]);
    assert_eq!(s.t.substitute(box_t, &sub), box_i, "Box[T] with T:=i64 is Box[i64]");
}

#[test]
fn substitute_reaches_into_an_arrow() {
    let mut s = s();
    let t = s.t.name("T");
    let v = s.t.var(t);
    let never = s.t.never();
    let arr_t = s.t.arrow(vec![v], never, v);
    let i = s.t.i64();
    let arr_i = s.t.arrow(vec![i], never, i);
    let sub = std::collections::HashMap::from([(t, i)]);
    assert_eq!(s.t.substitute(arr_t, &sub), arr_i);
}
