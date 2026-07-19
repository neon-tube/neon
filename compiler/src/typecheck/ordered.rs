//! Does a type have a structural order?
//!
//! `<` compares structure (see "Comparison is structural" in docs/decisions.md), so
//! ordering is not a property a type opts into — it is a property a type *has*, or does
//! not, by construction. This module is the single answer to that question, consulted
//! from three places that must never disagree:
//!
//! - the `<` operator on a concrete type, in `check.rs`;
//! - the `marker Ord` bound, discharged at each call site in `env.rs`;
//! - and mirrored by `has_order` in `backend/c.rs`, which panics rather than emitting a
//!   comparison it cannot make — so a gap here becomes a compiler crash, not a bad answer.
//!
//! **Order is infectious.** A container is ordered exactly when its contents are: a record
//! when every field is, a tuple when every element is, `List[T]` when `T` is. The types
//! with no order are the ones with nothing to compare or no canonical way to compare it —
//! a union (ranking its arms would be an invention), an atom (a name, not a magnitude),
//! `Map` and a self-referencing record (opaque pointers), a closure, and `null`.
//!
//! **A type variable is ordered when it is bound.** `bound` carries the type parameters the
//! enclosing signature declared `where T: Ord` for. Threading it through the *recursion*
//! rather than testing it only at the top is what makes the marker usable: without it
//! `Box[T]` under `where T: Ord` would stop at the field and report no order, and `Ord`
//! would be useless for anything but a bare `T`.

use super::bdd;
use super::env::Env;
use super::types::TyId;
use std::collections::HashSet;

/// Whether `ty` has a structural order, given the type parameters bound `Ord` here.
pub(super) fn is_ordered(env: &Env, ty: TyId, bound: &HashSet<String>) -> bool {
    ordered_rec(env, ty, bound, &mut Vec::new())
}

/// Whether `==` can compare `ty` structurally.
///
/// Equality is meant to be total (docs/decisions.md), and it nearly is: primitives, `str`,
/// atoms, records, tuples, `List` and `Map` all compare by content. This began as four
/// rejections; three have since been closed by giving the backend the comparison it was
/// missing (`Map` in 00b0640, a self-referencing record in e3e7b48, a `List` behind `null`
/// in b0e5bcf), and each bullet was deleted with the fix. Two remain:
///
/// - a **closure**: no structural answer exists, and C cannot `==` a `neon_closure`. This
///   one is permanent.
/// - a **union of two different records**: `record_fields` below reads a *single* record
///   atom, and `A | B` normalises to the two BDD paths `A` and `B & !A`, so it answers
///   `None` and the type is refused. Verified 2026-07-19: relaxing it to walk every path
///   is not enough, because the second path carries a negative. The message this produces
///   is honest about the shape now, and it is a diagnostic rather than a wrong answer --
///   but the doc line above still claims unions compare by content, and for two records
///   they do not.
///
/// Unlike ordering there is no bound to escape through, because equality takes none: a bare
/// type variable is *allowed* and deferred. Equality is total by design, so requiring a
/// marker would contradict the decision; the residual hole is the generic-instantiation one
/// already recorded in finalpush.md.
pub(super) fn is_equatable(env: &Env, ty: TyId) -> bool {
    equatable_rec(env, ty, &mut Vec::new())
}

fn equatable_rec(env: &Env, ty: TyId, seen: &mut Vec<TyId>) -> bool {
    let t = &env.solver.t;
    let d = t.data(ty);

    // A closure has no structural equality, at any depth.
    if d.arrows != bdd::FALSE {
        return false;
    }
    // A generic parameter is deferred: equality needs no bound.
    if !t.atomset_of(d.vars).is_empty_set() {
        return true;
    }
    let has_records = d.records != bdd::FALSE;
    if !has_records && d.tuples == bdd::FALSE {
        return true; // primitives, `str`, atoms, `null`
    }
    if seen.contains(&ty) {
        // A self-referencing record: equatable, and the *backend* handles the recursion
        // with a generated function. Answering `true` here only stops this type-level walk
        // from following the cycle forever.
        return true;
    }
    seen.push(ty);
    let ok = if d.tuples != bdd::FALSE {
        match tuple_elems(env, ty) {
            Some(elems) => elems.iter().all(|&e| equatable_rec(env, e, seen)),
            None => false,
        }
    } else {
        match super::nominal_head_of(env, ty).as_deref() {
            // A map compares by content, so its *values* must be comparable. Its keys
            // already are -- a key type without content equality could not be hashed into
            // the table in the first place.
            Some("Map") => match arg_of(env, ty, 1) {
                Some(val) => equatable_rec(env, val, seen),
                None => false,
            },
            Some("List") => match arg_of(env, ty, 0) {
                Some(elem) => equatable_rec(env, elem, seen),
                None => false,
            },
            _ => match record_fields(env, ty) {
                Some(fields) => fields.iter().all(|&(_, ft)| equatable_rec(env, ft, seen)),
                None => false,
            },
        }
    };
    seen.pop();
    ok
}

fn ordered_rec(env: &Env, ty: TyId, bound: &HashSet<String>, seen: &mut Vec<TyId>) -> bool {
    let t = &env.solver.t;
    let d = t.data(ty);

    // A rigid variable is ordered exactly when the signature said so.
    let vars = t.atomset_of(d.vars);
    if !vars.is_empty_set() {
        // Only a bare variable can be judged; a variable mixed with anything else is a
        // union, which has no order regardless.
        if d.base != 0
            || !t.atomset_of(d.atoms).is_empty_set()
            || d.records != bdd::FALSE
            || d.tuples != bdd::FALSE
            || d.arrows != bdd::FALSE
        {
            return false;
        }
        if vars.neg || vars.names.len() != 1 {
            return false;
        }
        return bound.contains(t.name_str(vars.names[0]));
    }

    if d.base & super::types::B_NULL != 0
        || !t.atomset_of(d.atoms).is_empty_set()
        || d.arrows != bdd::FALSE
    {
        return false;
    }
    let bases = (d.base
        & (super::types::B_I64 | super::types::B_F64 | super::types::B_STR | super::types::B_BOOL))
        .count_ones();
    let has_records = d.records != bdd::FALSE;
    let has_tuples = d.tuples != bdd::FALSE;
    // More than one shape is a union: no rank between the arms, so no order.
    if bases + u32::from(has_records) + u32::from(has_tuples) != 1 {
        return false;
    }
    if !has_records && !has_tuples {
        return true; // exactly one primitive base
    }
    // Reaching the same type again means it is pointer-backed, with no finite structure
    // to walk. Answering "no" also terminates the recursion.
    if seen.contains(&ty) {
        return false;
    }
    seen.push(ty);
    let ordered = if has_tuples {
        match tuple_elems(env, ty) {
            Some(elems) => elems.iter().all(|&e| ordered_rec(env, e, bound, seen)),
            None => false,
        }
    } else {
        match super::nominal_head_of(env, ty).as_deref() {
            // Opaque and pointer-backed: no elements reachable, nothing to compare.
            Some("Map") => false,
            Some("List") => match arg_of(env, ty, 0) {
                Some(elem) => ordered_rec(env, elem, bound, seen),
                None => false,
            },
            _ => match record_fields(env, ty) {
                Some(fields) => fields.iter().all(|&(_, ft)| ordered_rec(env, ft, bound, seen)),
                None => false,
            },
        }
    };
    seen.pop();
    ordered
}

/// The declared fields of a single record atom, dropping the reserved `#nominal` and
/// `#0`/`#1` generic-argument slots. `None` when `ty` is not exactly one record.
fn record_fields(env: &Env, ty: TyId) -> Option<Vec<(String, TyId)>> {
    let t = &env.solver.t;
    let d = t.data(ty);
    match t.rec_bdd.paths(d.records).as_slice() {
        [(pos, neg)] if neg.is_empty() && pos.len() == 1 => Some(
            t.rec_atoms[pos[0] as usize]
                .fields
                .iter()
                .map(|&(l, ft)| (t.name_str(l).to_string(), ft))
                .filter(|(n, _)| !n.starts_with('#'))
                .collect(),
        ),
        _ => None,
    }
}

/// The element types of a single tuple atom; `None` when `ty` is not exactly one.
fn tuple_elems(env: &Env, ty: TyId) -> Option<Vec<TyId>> {
    let t = &env.solver.t;
    let d = t.data(ty);
    match t.tup_bdd.paths(d.tuples).as_slice() {
        [(pos, neg)] if neg.is_empty() && pos.len() == 1 => {
            Some(t.tup_atoms[pos[0] as usize].elems.clone())
        }
        _ => None,
    }
}

/// Generic argument `i` of a nominal type, read from its reserved `#i` slot.
fn arg_of(env: &Env, ty: TyId, i: usize) -> Option<TyId> {
    let t = &env.solver.t;
    let d = t.data(ty);
    match t.rec_bdd.paths(d.records).as_slice() {
        [(pos, neg)] if neg.is_empty() && pos.len() == 1 => {
            let want = format!("#{i}");
            t.rec_atoms[pos[0] as usize]
                .fields
                .iter()
                .find(|&&(l, _)| t.name_str(l) == want)
                .map(|&(_, ft)| ft)
        }
        _ => None,
    }
}
