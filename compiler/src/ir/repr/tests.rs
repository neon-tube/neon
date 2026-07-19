use super::*;
use crate::typecheck::types::Types;

fn t() -> Types {
    Types::new()
}

#[test]
fn scalars_map_to_themselves() {
    let mut ty = t();
    let i = ty.i64();
    let f = ty.f64();
    let s = ty.str();
    let b = ty.bool();
    let n = ty.null();
    assert_eq!(repr_of(&ty, i), Repr::I64);
    assert_eq!(repr_of(&ty, f), Repr::F64);
    assert_eq!(repr_of(&ty, s), Repr::Str);
    assert_eq!(repr_of(&ty, b), Repr::Bool);
    assert_eq!(repr_of(&ty, n), Repr::Null);
}

#[test]
fn an_atom_and_a_union_of_atoms_are_one_tag() {
    let mut ty = t();
    let ok = ty.name("ok");
    let err = ty.name("err");
    let a = ty.atom(ok);
    let b = ty.atom(err);
    let u = ty.union(a, b);
    assert_eq!(repr_of(&ty, a), Repr::Tag);
    assert_eq!(repr_of(&ty, u), Repr::Tag, "a union of atoms is still one discriminant");
}

#[test]
fn the_empty_tuple_is_unit() {
    let mut ty = t();
    let unit = ty.tuple(vec![]);
    assert_eq!(repr_of(&ty, unit), Repr::Unit);
}

#[test]
fn a_tuple_is_its_elements_in_order() {
    let mut ty = t();
    let i = ty.i64();
    let s = ty.str();
    let tup = ty.tuple(vec![i, s]);
    assert_eq!(repr_of(&ty, tup), Repr::Tuple(vec![Repr::I64, Repr::Str]));
}

#[test]
fn a_struct_keeps_its_fields_and_drops_the_metadata() {
    let mut ty = t();
    let i = ty.i64();
    let s = ty.str();
    let a = ty.name("a");
    let b = ty.name("b");
    // An anonymous record `{ a: i64, b: str }` — struct_ty adds the `#nominal` tag.
    let rec = ty.struct_ty(vec![(a, i), (b, s)]);
    match repr_of(&ty, rec) {
        Repr::Record { name, fields } => {
            assert!(name.is_none(), "anonymous record has no nominal name");
            assert_eq!(
                fields,
                vec![("a".to_string(), Repr::I64), ("b".to_string(), Repr::Str)],
                "metadata labels (#nominal) are not fields"
            );
        }
        other => panic!("expected a record, got {other:?}"),
    }
}

#[test]
fn a_nominal_record_keeps_its_name() {
    let mut ty = t();
    let i = ty.i64();
    let r = ty.name("r");
    let circle = ty.name("Circle");
    let nom = ty.nominal(circle, vec![], vec![(r, i)]);
    match repr_of(&ty, nom) {
        Repr::Record { name, fields } => {
            assert_eq!(name.as_deref(), Some("Circle"));
            assert_eq!(fields, vec![("r".to_string(), Repr::I64)]);
        }
        other => panic!("expected a record, got {other:?}"),
    }
}

#[test]
fn list_and_map_are_runtime_containers() {
    let mut ty = t();
    let i = ty.i64();
    let s = ty.str();
    let list_name = ty.name("List");
    let list = ty.nominal(list_name, vec![i], vec![]);
    assert_eq!(repr_of(&ty, list), Repr::List(Box::new(Repr::I64)));

    let map_name = ty.name("Map");
    let map = ty.nominal(map_name, vec![s, i], vec![]);
    assert_eq!(repr_of(&ty, map), Repr::Map(Box::new(Repr::Str), Box::new(Repr::I64)));
}

#[test]
fn nullable_of_a_pointer_is_a_nullable_pointer() {
    let mut ty = t();
    let s = ty.str();
    let n = ty.null();
    let s_or_null = ty.union(s, n);
    assert_eq!(repr_of(&ty, s_or_null), Repr::Nullable(Box::new(Repr::Str)));
}

#[test]
fn nullable_of_a_scalar_is_a_union() {
    let mut ty = t();
    let i = ty.i64();
    let n = ty.null();
    let i_or_null = ty.union(i, n);
    // `i64` is not pointer-backed, so it needs a discriminant, not a null pointer.
    match repr_of(&ty, i_or_null) {
        Repr::Union(vs) => assert!(vs.contains(&Repr::I64) && vs.contains(&Repr::Null)),
        other => panic!("expected a union, got {other:?}"),
    }
}

#[test]
fn a_sum_of_records_is_a_union() {
    let mut ty = t();
    let i = ty.i64();
    let circle = ty.name("Circle");
    let rect = ty.name("Rect");
    let r_field = ty.name("r");
    let w_field = ty.name("w");
    let c = ty.nominal(circle, vec![], vec![(r_field, i)]);
    let r = ty.nominal(rect, vec![], vec![(w_field, i)]);
    let shape = ty.union(c, r);
    match repr_of(&ty, shape) {
        Repr::Union(vs) => assert_eq!(vs.len(), 2, "Circle | Rect has two variants"),
        other => panic!("expected a union, got {other:?}"),
    }
}

#[test]
fn an_arrow_is_a_closure() {
    let mut ty = t();
    let i = ty.i64();
    let s = ty.str();
    let never = ty.never();
    let arrow = ty.arrow(vec![i], never, s);
    assert_eq!(
        repr_of(&ty, arrow),
        Repr::Closure {
            params: vec![Repr::I64],
            throws: Box::new(Repr::Never),
            ret: Box::new(Repr::Str),
        }
    );
}

#[test]
fn a_throwing_arrow_carries_its_throws() {
    let mut ty = t();
    let i = ty.i64();
    let s = ty.str();
    let err = ty.name("err");
    let err = ty.atom(err);
    let arrow = ty.arrow(vec![i], err, s);
    assert_eq!(
        repr_of(&ty, arrow),
        Repr::Closure {
            params: vec![Repr::I64],
            throws: Box::new(Repr::Tag),
            ret: Box::new(Repr::Str),
        }
    );
}

#[test]
fn a_rigid_variable_is_abstract() {
    let mut ty = t();
    let tn = ty.name("T");
    let var = ty.var(tn);
    assert_eq!(repr_of(&ty, var), Repr::Var("T".to_string()));
    assert!(!repr_of(&ty, var).is_concrete());
}

#[test]
fn never_maps_to_never() {
    let mut ty = t();
    let n = ty.never();
    assert_eq!(repr_of(&ty, n), Repr::Never);
}

/// One representative of every `Repr` variant. Adding a variant breaks this match, which
/// is the point: the three predicates below decide whether the refcount pass tracks a
/// value, and a variant that slips in through a `_` arm silently answers "no".
fn one_of_each() -> Vec<(&'static str, Repr)> {
    let all = vec![
        ("I64", Repr::I64),
        ("F64", Repr::F64),
        ("Bool", Repr::Bool),
        ("Str", Repr::Str),
        ("Null", Repr::Null),
        ("Unit", Repr::Unit),
        ("Tag", Repr::Tag),
        ("Record", Repr::Record { name: None, fields: vec![("a".into(), Repr::I64)] }),
        ("Tuple", Repr::Tuple(vec![Repr::I64])),
        ("List", Repr::List(Box::new(Repr::I64))),
        ("Map", Repr::Map(Box::new(Repr::Str), Box::new(Repr::I64))),
        (
            "Runtime",
            Repr::Runtime { nominal: "R".into(), c_type: "neon_r".into(), args: vec![Repr::I64] },
        ),
        (
            "Closure",
            Repr::Closure {
                params: vec![Repr::I64],
                throws: Box::new(Repr::Never),
                ret: Box::new(Repr::I64),
            },
        ),
        ("Union", Repr::Union(vec![Repr::I64, Repr::Bool])),
        ("Nullable", Repr::Nullable(Box::new(Repr::Str))),
        ("Var", Repr::Var("T".into())),
        ("BoxedRec", Repr::BoxedRec(0)),
        ("Recursive", Repr::Recursive(crate::typecheck::types::TyId(0))),
        ("Any", Repr::Any),
        ("Never", Repr::Never),
    ];
    // Exhaustiveness: every name above is a distinct variant, and the compiler forces the
    // list to be revisited when one is added.
    for (_, r) in &all {
        match r {
            Repr::I64
            | Repr::F64
            | Repr::Bool
            | Repr::Str
            | Repr::Null
            | Repr::Unit
            | Repr::Tag
            | Repr::Record { .. }
            | Repr::Tuple(_)
            | Repr::List(_)
            | Repr::Map(_, _)
            | Repr::Runtime { .. }
            | Repr::Closure { .. }
            | Repr::Union(_)
            | Repr::Nullable(_)
            | Repr::Var(_)
            | Repr::BoxedRec(_)
            | Repr::Recursive(_)
            | Repr::Any
            | Repr::Never => {}
        }
    }
    all
}

/// The exact set of reprs that live behind a pointer, so `T | null` may use a null pointer
/// rather than a discriminant. `Recursive` is deliberately absent — it names a type without
/// describing it, and the type it names may be an inline union.
#[test]
fn is_pointer_is_pinned_per_variant() {
    let expected = ["Str", "List", "Map", "Runtime", "Closure", "BoxedRec"];
    for (name, r) in one_of_each() {
        assert_eq!(
            r.is_pointer(),
            expected.contains(&name),
            "is_pointer disagrees for {name}: {r:?}"
        );
    }
}

/// The predicate the refcount pass gates on. Getting it wrong in one direction leaks and
/// in the other frees something live, so it is pinned variant by variant rather than
/// spot-checked. `Any` counts without being a pointer (it is a box with a header);
/// aggregates count when anything inside them does.
#[test]
fn is_counted_is_pinned_per_variant() {
    let expected =
        ["Str", "List", "Map", "Runtime", "Closure", "BoxedRec", "Any", "Recursive", "Nullable"];
    for (name, r) in one_of_each() {
        assert_eq!(r.is_counted(), expected.contains(&name), "is_counted disagrees for {name}");
    }
    // Aggregates are counted exactly when a part is.
    assert!(!Repr::Tuple(vec![Repr::I64, Repr::Bool]).is_counted());
    assert!(Repr::Tuple(vec![Repr::I64, Repr::Str]).is_counted());
    assert!(!Repr::Record { name: None, fields: vec![("a".into(), Repr::I64)] }.is_counted());
    assert!(Repr::Record { name: None, fields: vec![("a".into(), Repr::List(Box::new(Repr::I64)))] }
        .is_counted());
    assert!(!Repr::Union(vec![Repr::I64, Repr::Null]).is_counted());
    assert!(Repr::Union(vec![Repr::I64, Repr::Str]).is_counted());
    assert!(!Repr::Nullable(Box::new(Repr::Unit)).is_counted());
}

/// `Var` is the only thing that is not concrete. `Recursive` is concrete — a resolved
/// back-edge, not an unknown — and every aggregate is concrete exactly when its parts are.
#[test]
fn is_concrete_is_pinned_per_variant() {
    for (name, r) in one_of_each() {
        assert_eq!(r.is_concrete(), name != "Var", "is_concrete disagrees for {name}");
    }
    let v = Repr::Var("T".into());
    assert!(!Repr::List(Box::new(v.clone())).is_concrete());
    assert!(!Repr::Map(Box::new(Repr::I64), Box::new(v.clone())).is_concrete());
    assert!(!Repr::Nullable(Box::new(v.clone())).is_concrete());
    assert!(!Repr::Tuple(vec![Repr::I64, v.clone()]).is_concrete());
    assert!(!Repr::Union(vec![Repr::I64, v.clone()]).is_concrete());
    assert!(!Repr::Record { name: None, fields: vec![("a".into(), v.clone())] }.is_concrete());
    assert!(!Repr::Runtime {
        nominal: "R".into(),
        c_type: "neon_r".into(),
        args: vec![v.clone()],
    }
    .is_concrete());
    assert!(!Repr::Closure {
        params: vec![],
        throws: Box::new(v.clone()),
        ret: Box::new(Repr::I64),
    }
    .is_concrete());
    assert!(!Repr::Closure { params: vec![], throws: Box::new(Repr::Never), ret: Box::new(v) }
        .is_concrete());
}

/// `normalize_union` claims to reproduce the order `repr_components` pushes its components
/// in. It did not: `List`, `Map`, `Runtime` and `BoxedRec` all come out of the *records*
/// phase, before tuples and arrows, but fell into `variant_rank`'s catch-all and sorted
/// after them. One type, two variant orders — which is two C structs the backend refuses to
/// assign between.
#[test]
fn normalize_union_reproduces_the_order_repr_of_derives() {
    let mut ty = t();
    let i = ty.i64();
    let s = ty.str();
    let ln = ty.name("List");
    let list = ty.nominal(ln, vec![i], vec![]);
    let tup = ty.tuple(vec![i, s]);
    let u = ty.union(list, tup);

    let direct = repr_of(&ty, u);
    assert_eq!(
        direct,
        Repr::Union(vec![
            Repr::List(Box::new(Repr::I64)),
            Repr::Tuple(vec![Repr::I64, Repr::Str]),
        ]),
        "the records phase runs before the tuples phase"
    );
    // Whatever order an instance's variants arrive in, re-normalising must land back on the
    // one `repr_of` derives for the same type.
    for start in [
        vec![Repr::List(Box::new(Repr::I64)), Repr::Tuple(vec![Repr::I64, Repr::Str])],
        vec![Repr::Tuple(vec![Repr::I64, Repr::Str]), Repr::List(Box::new(Repr::I64))],
    ] {
        assert_eq!(normalize_union(start.clone()), direct, "normalize_union({start:?})");
    }
}

/// An arrow's `throws` is a real edge of the type graph, exactly as its params and return
/// are. It was missing from `successors`, so a cycle closing through the clause was never
/// cut and `repr_of` recursed until the stack ran out.
#[test]
fn a_cycle_through_an_arrows_throws_clause_is_cut() {
    // `mu F = null | (i64) throws F -> i64` — the back-edge is the error clause.
    let mut ty = t();
    let i = ty.i64();
    let f = ty.reserve();
    let arrow = ty.arrow(vec![i], f, i);
    let n = ty.null();
    let body = ty.union(n, arrow);
    ty.define(f, ty.data(body));
    assert!(ty.all_defined());
    // The whole assertion is that this returns at all.
    let r = repr_of(&ty, f);
    assert!(r.is_concrete(), "a recursive arrow type is concrete, got {r:?}");
}

