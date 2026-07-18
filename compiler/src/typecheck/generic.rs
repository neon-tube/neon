//! Inferring a generic call's type arguments by structural matching.
//!
//! `list::get(xs, 0)` with `xs: List[i64]` matches the parameter template `List[T]`
//! against `List[i64]` and binds `T := i64`. This is matching, not the full
//! subtype inference of Castagna — it decomposes a nominal, tuple or arrow
//! structurally and binds a bare variable where it meets one, which is all a
//! function signature needs.

use super::types::{NameId, TyId, Types};
use std::collections::{HashMap, HashSet};

/// Refine `subst` so `template[subst]` can match `concrete`. A variable binds to the
/// first concrete type it meets and stays there — it is not widened to the union of
/// later matches. So `push(xs: List[i64], "s")` pins `T := i64` from the list, and
/// the `str` argument is then a mismatch rather than a silent widening to
/// `List[i64|str]`. Widening a generic is explicit: a turbofish or the expected
/// type, applied before inference. See docs/decisions.md.
pub fn infer(
    t: &mut Types,
    template: TyId,
    concrete: TyId,
    vars: &HashSet<NameId>,
    subst: &mut HashMap<NameId, TyId>,
) {
    if template == concrete {
        return;
    }
    if let Some(v) = as_var(t, template) {
        if vars.contains(&v) {
            subst.entry(v).or_insert(concrete);
            return;
        }
    }
    // Decompose matching structures. A mismatch is not an error here: the
    // substituted signature is checked against the arguments by the caller, which
    // is where a real mismatch is reported.
    if let (Some(a), Some(b)) = (single_record(t, template), single_record(t, concrete)) {
        for (label, tf) in a {
            if let Some(cf) = b.iter().find(|(l, _)| *l == label).map(|(_, f)| *f) {
                infer(t, tf, cf, vars, subst);
            }
        }
    } else if let (Some(a), Some(b)) = (single_tuple(t, template), single_tuple(t, concrete)) {
        if a.len() == b.len() {
            for (tf, cf) in a.into_iter().zip(b) {
                infer(t, tf, cf, vars, subst);
            }
        }
    } else if let (Some(a), Some(b)) = (single_arrow(t, template), single_arrow(t, concrete)) {
        if a.0.len() == b.0.len() {
            for (tp, cp) in a.0.into_iter().zip(b.0) {
                infer(t, tp, cp, vars, subst);
            }
            infer(t, a.1, b.1, vars, subst);
            infer(t, a.2, b.2, vars, subst);
        }
    }
}

/// The variable name when `ty` is exactly one rigid variable and nothing else.
fn as_var(t: &Types, ty: TyId) -> Option<NameId> {
    let d = t.data(ty);
    if d.base != 0 || !t.atomset_of(d.atoms).is_empty_set() {
        return None;
    }
    if !bdd_empty(d.records) || !bdd_empty(d.tuples) || !bdd_empty(d.arrows) {
        return None;
    }
    let vars = t.atomset_of(d.vars);
    (!vars.neg && vars.names.len() == 1).then(|| vars.names[0])
}

fn bdd_empty(b: super::bdd::BddId) -> bool {
    b == super::bdd::FALSE
}

/// The fields of `ty` when it is exactly one record atom (a nominal or a struct).
fn single_record(t: &Types, ty: TyId) -> Option<Vec<(NameId, TyId)>> {
    let d = t.data(ty);
    if d.base != 0 || !t.atomset_of(d.atoms).is_empty_set() || !t.atomset_of(d.vars).is_empty_set() {
        return None;
    }
    if !bdd_empty(d.tuples) || !bdd_empty(d.arrows) {
        return None;
    }
    match t.rec_bdd.paths(d.records).as_slice() {
        [(pos, neg)] if neg.is_empty() && pos.len() == 1 => {
            let a = &t.rec_atoms[pos[0] as usize];
            Some(a.fields.clone())
        }
        _ => None,
    }
}

fn single_tuple(t: &Types, ty: TyId) -> Option<Vec<TyId>> {
    let d = t.data(ty);
    if d.base != 0 || !bdd_empty(d.records) || !bdd_empty(d.arrows) {
        return None;
    }
    match t.tup_bdd.paths(d.tuples).as_slice() {
        [(pos, neg)] if neg.is_empty() && pos.len() == 1 => {
            Some(t.tup_atoms[pos[0] as usize].elems.clone())
        }
        _ => None,
    }
}

fn single_arrow(t: &Types, ty: TyId) -> Option<(Vec<TyId>, TyId, TyId)> {
    let d = t.data(ty);
    if d.base != 0 || !bdd_empty(d.records) || !bdd_empty(d.tuples) {
        return None;
    }
    match t.arrow_bdd.paths(d.arrows).as_slice() {
        [(pos, neg)] if neg.is_empty() && pos.len() == 1 => {
            let a = &t.arrow_atoms[pos[0] as usize];
            Some((a.params.clone(), a.throws, a.ret))
        }
        _ => None,
    }
}
