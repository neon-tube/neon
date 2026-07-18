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
        Repr::Closure { params: vec![Repr::I64], ret: Box::new(Repr::Str) }
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

