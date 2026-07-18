use super::generic::infer;
use super::types::Types;
use std::collections::{HashMap, HashSet};

fn setup() -> Types {
    Types::new()
}

#[test]
fn infer_binds_a_bare_variable() {
    let mut t = setup();
    let tn = t.name("T");
    let v = t.var(tn);
    let i = t.i64();
    let vars = HashSet::from([tn]);
    let mut subst = HashMap::new();
    infer(&mut t, v, i, &vars, &mut subst);
    assert_eq!(subst.get(&tn), Some(&i));
}

#[test]
fn infer_reaches_into_a_generic_argument() {
    let mut t = setup();
    let tn = t.name("T");
    let v = t.var(tn);
    let list_t = { let n = t.name("List"); t.nominal(n, vec![v], vec![]) };
    let i = t.i64();
    let list_i = { let n = t.name("List"); t.nominal(n, vec![i], vec![]) };
    let vars = HashSet::from([tn]);
    let mut subst = HashMap::new();
    infer(&mut t, list_t, list_i, &vars, &mut subst);
    assert_eq!(subst.get(&tn), Some(&i), "T inferred from List[T] vs List[i64]");
}

#[test]
fn infer_keeps_the_first_binding_of_a_variable() {
    // Strict: the first match pins T; a later disagreeing match does not widen it.
    // The mismatch is caught by checking the argument against the substituted
    // signature, not by silently unioning the two.
    let mut t = setup();
    let tn = t.name("T");
    let v = t.var(tn);
    let i = t.i64();
    let st = t.str();
    let vars = HashSet::from([tn]);
    let mut subst = HashMap::new();
    infer(&mut t, v, i, &vars, &mut subst);
    infer(&mut t, v, st, &vars, &mut subst);
    assert_eq!(subst.get(&tn), Some(&i), "T stays i64, not i64 | str");
}
