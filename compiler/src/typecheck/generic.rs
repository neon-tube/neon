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
    // Identical types still have work to do when the template mentions a variable being
    // solved. A generic that calls another generic passes its *own* rigid `T`, so the
    // template `List[T]` and the argument `List[T]` are the same `TyId` -- and returning
    // here bound nothing, leaving the callee unmonomorphised. Codegen then laid its
    // instance out generically and read every element at the wrong width: `sort_by` on a
    // list of `str` corrupted the heap. The early-out is still worth having for the common
    // case of a fully concrete argument.
    if template == concrete && !mentions_var(t, template, vars, &mut Vec::new()) {
        return;
    }
    if let Some(v) = as_var(t, template) {
        if vars.contains(&v) {
            // First binding wins -- that is what makes inference top-down, so an expected
            // type pins a variable and the arguments conform to it.
            //
            // The exception is a variable bound to *itself*. A generic calling a generic
            // passes its own rigid `T`, which yields `T := T`: real, and necessary, since
            // it is what tells lowering to resolve the callee at the enclosing instance's
            // binding. But it teaches nothing about the type, so it must not out-rank a
            // concrete answer found later. Nested `map::set` calls produce exactly that
            // race -- the inner call's `Map[K, V]` return and the outer's parameter are
            // the same `TyId`, so `K := K` landed first and blocked `K := str`.
            let self_bound = |ty| as_var(t, ty) == Some(v);
            match subst.get(&v).copied() {
                None => {
                    subst.insert(v, concrete);
                }
                Some(prev) if self_bound(prev) && !self_bound(concrete) => {
                    subst.insert(v, concrete);
                }
                _ => {}
            }
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

/// True when `ty` is a rigid variable (so a bound cannot yet be discharged).
/// Whether `ty` mentions any of `vars`, at any depth. `seen` breaks the cycle on a
/// recursive type, which would otherwise walk itself forever.
fn mentions_var(t: &Types, ty: TyId, vars: &HashSet<NameId>, seen: &mut Vec<TyId>) -> bool {
    if seen.contains(&ty) {
        return false;
    }
    // A variable mixed with anything else (`T | null`) is not a bare var, so check the
    // variable set directly rather than going through `as_var`.
    let names = t.atomset_of(t.data(ty).vars);
    if names.names.iter().any(|n| vars.contains(n)) {
        return true;
    }
    seen.push(ty);
    let found = if let Some(fields) = single_record(t, ty) {
        fields.iter().any(|&(_, f)| mentions_var(t, f, vars, seen))
    } else if let Some(elems) = single_tuple(t, ty) {
        elems.iter().any(|&e| mentions_var(t, e, vars, seen))
    } else if let Some((params, ret, throws)) = single_arrow(t, ty) {
        params.iter().any(|&p| mentions_var(t, p, vars, seen))
            || mentions_var(t, ret, vars, seen)
            || mentions_var(t, throws, vars, seen)
    } else {
        false
    };
    seen.pop();
    found
}

pub fn is_var(t: &Types, ty: TyId) -> bool {
    as_var(t, ty).is_some()
}

/// The variable name when `ty` is exactly one rigid variable and nothing else.
pub(super) fn as_var_name(t: &Types, ty: TyId) -> Option<NameId> {
    as_var(t, ty)
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
///
/// The `no negatives` half of the test is not the conservatism it looks like. A narrowed
/// match arm used to arrive here as `Map[str, i64] & !List[i64]` and be declined, but the
/// fix for that belongs in `Solver::prune`, which drops a negation that subtracts nothing
/// before it ever reaches a consumer — so by the time inference sees a narrowed value the
/// path is genuinely a single atom. A negative that survives pruning is a real one, and a
/// type that really is `A ∧ ¬B` has no single set of fields to hand back.
fn single_record(t: &Types, ty: TyId) -> Option<Vec<(NameId, TyId)>> {
    let d = t.data(ty);
    if d.base != 0 || !t.atomset_of(d.atoms).is_empty_set() || !t.atomset_of(d.vars).is_empty_set()
    {
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
