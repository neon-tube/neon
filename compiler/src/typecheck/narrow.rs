//! Pattern and condition refinement, and exhaustiveness.
//!
//! Narrowing is a set operation: `is Circle` splits the subject into `s ∧ Circle` and
//! `s ∧ ¬Circle`. Exhaustiveness is the same operation run over every arm at once —
//! the match covers `s` iff `s ∧ ¬(⋁ arms)` is empty — and the residual of that
//! subtraction is a type naming exactly what was missed.
//!
//! Everything here is `TyId` in and `TyId` out. Turning an `ast::TypeSpec` or an
//! `ast::Pattern` into a `TyId` is `resolve.rs`'s job.
//!
//! # An empty branch is a diagnostic, never dead code
//!
//! This module deliberately does not hand back a `(then, else)` pair, because an empty
//! side of one is a **trap**:
//!
//! ```text
//! fn g(s: str) -> str { s }
//! fn f[T](x: T) -> str {
//!     if x is i64 { g(x) } else { "no" }
//! }
//! ```
//!
//! A rigid `T` is disjoint from `i64`, so the then-branch binding is `T ∧ i64 = never`.
//! But `never <: str` — `never` is below *everything* — so `g(x)` typechecks
//! vacuously. Call `f(5)` with `T := i64` and the branch is live at runtime, handing
//! `g` an i64 where it wants a str. `T` was only opaque, not uninhabited.
//!
//! So a `never` binding makes every downstream check succeed for the wrong reason.
//! This is the trap `typechecker.md` describes for error recovery, and it has the same
//! answer: that is exactly why `Descriptor::Error` is poison and pointedly *not*
//! `never`. An impossible test must reach the user as "this test can never succeed",
//! not silently type a branch nobody looked at.
//!
//! [`Refined`] is therefore an enum with no `then_ty` to read on the impossible case,
//! and [`Projected`] is the same discipline for a field that might not be there. A
//! caller that ignores the empty case does not compile.

use super::empty::Solver;
use super::types::*;
use std::collections::HashMap;

/// Mirrors `empty.rs`: the label standing for every field no atom on the path names.
const REST: NameId = NameId(u32::MAX);

/// The outcome of a refinement.
///
/// Deliberately not a struct of two types: see the module docs. Only [`Refined::Both`]
/// carries a binding, and both of its types are guaranteed inhabited, so there is no
/// way to obtain a `never` binding from this module by accident.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refined {
    /// Both branches are live. Both types are inhabited.
    Both { then_ty: TyId, else_ty: TyId },
    /// The test always succeeds; there is no `else` branch to check. The payload is
    /// the subject, unrefined.
    ///
    /// Report it: a test that cannot fail is a mistake, and checking the `else` with a
    /// `never` binding would hide whatever is written in it.
    AlwaysMatches(TyId),
    /// The test can never succeed; there is no `then` branch to check. The payload is
    /// the subject, unrefined.
    ///
    /// **Report this.** It is `x is str` on an `i64`, and it is `x is i64` on a rigid
    /// `T` — the second of which is live at runtime once `T` is instantiated.
    NeverMatches(TyId),
    /// The subject was already empty: the code before the test is unreachable, and the
    /// diagnostic for that belongs where it became empty rather than here.
    Unreachable,
}

impl Refined {
    /// Both branches, or `None` when either side is empty. The caller must have an
    /// answer for the empty case before it can bind anything.
    pub fn both(self) -> Option<(TyId, TyId)> {
        match self {
            Refined::Both { then_ty, else_ty } => Some((then_ty, else_ty)),
            _ => None,
        }
    }

    /// Swaps the branches: `!(x is T)`, or `!= null` against `== null`.
    pub fn flip(self) -> Refined {
        match self {
            Refined::Both { then_ty, else_ty } => {
                Refined::Both { then_ty: else_ty, else_ty: then_ty }
            }
            Refined::AlwaysMatches(t) => Refined::NeverMatches(t),
            Refined::NeverMatches(t) => Refined::AlwaysMatches(t),
            Refined::Unreachable => Refined::Unreachable,
        }
    }
}

/// The one place emptiness is decided, so `Both` being inhabited on both sides holds
/// by construction.
fn refined(s: &mut Solver, subject: TyId, then_ty: TyId, else_ty: TyId) -> Refined {
    match (s.is_empty(then_ty), s.is_empty(else_ty)) {
        (true, true) => Refined::Unreachable,
        (true, false) => Refined::NeverMatches(subject),
        (false, true) => Refined::AlwaysMatches(subject),
        (false, false) => Refined::Both { then_ty, else_ty },
    }
}

/// What a pattern or condition tests for.
///
/// `exact` is the difference between "matched, so the value is a `T`" and, on top of
/// that, "did not match, so the value is not a `T`". The literal `1` is an `i64` when
/// it matches, but the values it rejects are `i64` too, so it subtracts nothing from
/// the fallthrough and covers nothing. Same for a guarded arm. Only a test that admits
/// *every* value of its type — `is T`, `:ok`, `null`, `_` — is exact.
///
/// Without this distinction `match n { 1 => .. }` would report as exhaustive.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Test {
    pub ty: TyId,
    pub exact: bool,
}

impl Test {
    pub fn exact(ty: TyId) -> Test {
        Test { ty, exact: true }
    }

    pub fn inexact(ty: TyId) -> Test {
        Test { ty, exact: false }
    }

    /// A guard can always reject, so it makes an otherwise exact test inexact.
    pub fn guarded(self) -> Test {
        Test { ty: self.ty, exact: false }
    }

    /// What an arm with this test removes from the fallthrough, and contributes to
    /// coverage.
    pub fn covered(self, s: &mut Solver) -> TyId {
        if self.exact { self.ty } else { s.t.never() }
    }
}

// ---- conditions ----

/// `x is T`. Splits into `s ∧ T` and `s ∧ ¬T`.
///
/// `¬T` spans the field lattice, `undef` included, but the intersection with `s` puts
/// the result back in the value lattice — so neither branch can pick up a "field
/// absent" marker the subject did not already carry.
pub fn narrow_is(s: &mut Solver, subject: TyId, ty: TyId) -> Refined {
    let then_ty = s.t.intersect(subject, ty);
    let else_ty = s.t.diff(subject, ty);
    refined(s, subject, then_ty, else_ty)
}

/// Narrowing by an arbitrary [`Test`], which is `narrow_is` only when the test is exact.
///
/// The inexact case is still worth running: the then-branch is genuinely refined, and an
/// impossible test is still reported. `match c { 1 => .. }` on a record is
/// [`Refined::NeverMatches`] even though a literal covers nothing — inexactness is about
/// what an arm *removes*, not about whether it can run at all.
pub fn narrow(s: &mut Solver, subject: TyId, test: Test) -> Refined {
    if test.exact {
        return narrow_is(s, subject, test.ty);
    }
    // An inexact test rejects values its own type still contains, so the fallthrough
    // keeps the whole subject and the two branches overlap.
    let then_ty = s.t.intersect(subject, test.ty);
    refined(s, subject, then_ty, subject)
}

/// `x == null`.
pub fn narrow_null(s: &mut Solver, subject: TyId) -> Refined {
    let n = s.t.null();
    narrow_is(s, subject, n)
}

/// `x != null`, and the left operand of `orelse`. `T | null` narrows to `T` with
/// nothing to unwrap.
pub fn narrow_not_null(s: &mut Solver, subject: TyId) -> Refined {
    narrow_null(s, subject).flip()
}

/// `:ok`. Atoms are singletons, so this is exact in both directions.
pub fn narrow_atom(s: &mut Solver, subject: TyId, name: NameId) -> Refined {
    let a = s.t.atom(name);
    narrow_is(s, subject, a)
}

// ---- patterns ----

/// The type a record pattern tests for: the fields it names, each at the type its
/// sub-pattern tests for, intersected with `tag` when the pattern names a record.
///
/// Open in the fields it does not name — that is what makes `Point { x }` match a
/// `Point` that also has a `y`.
pub fn record_test(s: &mut Solver, tag: Option<TyId>, fields: &[(NameId, TyId)]) -> TyId {
    let shape = s.t.struct_ty(fields.to_vec());
    match tag {
        Some(t) => s.t.intersect(shape, t),
        None => shape,
    }
}

/// The result of reading a field or element out of a subject.
///
/// Same discipline as [`Refined`]: the absent case has no type to bind, so a caller
/// cannot reach for one without deciding what to do about it. The payload is always a
/// *value* type — the `undef` marker is reported as `Partial`/`Absent` rather than
/// smuggled into a type the checker would go on to treat as a value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Projected {
    /// Every value in the subject has it, at this type.
    Present(TyId),
    /// Some values in the subject have it and some do not; the payload is the type it
    /// holds where it is present.
    ///
    /// Not an error by itself: `decisions.md` has a missing field satisfy a nullable
    /// one, so this is what an optional record field reads as. It *is* an error where
    /// null is not expected, which is the caller's call to make.
    Partial(TyId),
    /// Nothing in the subject has it — including the subject not being a record or a
    /// tuple at all. No type here, deliberately: `never` would check vacuously against
    /// whatever the field went on to be used as.
    Absent,
}

impl Projected {
    /// The one place the three cases are decided, so `Present`/`Partial` always carry an
    /// inhabited type. `found` holds a contribution per inhabited leaf of the walk and
    /// `lacks` says some leaf did not have the field at all; the union being empty means
    /// no leaf had it, which is `Absent` — including the case where the subject is not a
    /// record or tuple to begin with, since then there are no leaves to contribute.
    fn new(s: &mut Solver, found: Vec<TyId>, lacks: bool) -> Projected {
        let mut ty = s.t.never();
        for f in found {
            ty = s.t.union(ty, f);
        }
        if s.is_empty(ty) {
            return Projected::Absent;
        }
        if lacks { Projected::Partial(ty) } else { Projected::Present(ty) }
    }

    /// The type where present, or `None` when nothing has it.
    pub fn ty(self) -> Option<TyId> {
        match self {
            Projected::Present(t) | Projected::Partial(t) => Some(t),
            Projected::Absent => None,
        }
    }
}

/// The type of `label` over every inhabited record in `ty`.
pub fn project_field(s: &mut Solver, ty: TyId, label: NameId) -> Projected {
    let d = s.t.data(ty);
    let paths = s.t.rec_bdd.paths(d.records);
    let mut found = Vec::new();
    let mut lacks = false;
    for (pos, neg) in paths {
        rec_path_field(s, &pos, &neg, label, &mut found, &mut lacks);
    }
    Projected::new(s, found, lacks)
}

/// The field-wise decomposition of `empty.rs`, collecting `label` instead of deciding
/// a bool. A path contributes only where it is inhabited, so the projection never
/// names a type no value of `ty` actually holds.
fn rec_path_field(
    s: &mut Solver,
    pos: &[u32],
    neg: &[u32],
    label: NameId,
    found: &mut Vec<TyId>,
    lacks: &mut bool,
) {
    let mut labels: Vec<NameId> = vec![label];
    for &i in pos.iter().chain(neg) {
        labels.extend(s.t.rec_atoms[i as usize].labels());
    }
    labels.sort_unstable();
    labels.dedup();

    let full = s.t.any_or_undef();
    let mut map: HashMap<NameId, TyId> = labels.iter().map(|&l| (l, full)).collect();
    let mut rest = full;

    for &i in pos {
        let a = s.t.rec_atoms[i as usize].clone();
        for &l in &labels {
            let g = a.get(l);
            let cur = map[&l];
            let n = s.t.intersect(cur, g);
            map.insert(l, n);
        }
        rest = s.t.intersect(rest, a.rest);
    }

    for &l in &labels {
        // Slot-aware, as in `rec_path_empty`: a type-argument slot made empty only by
        // the recursion cycle must not kill the projection path, or every field of a
        // record whose recursion runs through a container reads as absent.
        if s.field_empty(l, map[&l]) {
            return;
        }
    }
    if s.is_empty(rest) {
        return;
    }

    map.insert(REST, rest);
    rec_neg_field(s, &labels, &map, neg, label, found, lacks);
}

/// Split the path's negative atoms out, one at a time, into the leaves where the
/// negation actually bites.
///
/// `¬N` for a record atom `N` is a disjunction over its labels — a value fails to be an
/// `N` by differing at *some* field — so each negative branches the walk once per label
/// (plus `REST`, the stand-in for every label no atom on the path names), refining that
/// one field by subtraction and leaving the others alone. Branches that refine to
/// nothing are dropped, which is what stops the fan-out from exploding and what makes a
/// negative that rules the path out entirely contribute no leaf at all.
///
/// Only leaves reached with every field inhabited get to say anything about `label`.
fn rec_neg_field(
    s: &mut Solver,
    labels: &[NameId],
    map: &HashMap<NameId, TyId>,
    neg: &[u32],
    label: NameId,
    found: &mut Vec<TyId>,
    lacks: &mut bool,
) {
    let Some((&s_id, tail)) = neg.split_first() else {
        // Every field on this leaf is inhabited, so split what it holds at `label`
        // into the value part and the absent marker. This is where `∧ any` is
        // load-bearing: it is what keeps `undef` out of the type the caller binds.
        let v = map[&label];
        let any = s.t.any();
        let undef = s.t.undef();
        let val = s.t.intersect(v, any);
        let ud = s.t.intersect(v, undef);
        if !s.is_empty(ud) {
            *lacks = true;
        }
        if !s.is_empty(val) {
            found.push(val);
        }
        return;
    };
    let sa = s.t.rec_atoms[s_id as usize].clone();

    for &l in labels.iter().chain(std::iter::once(&REST)) {
        let against = if l == REST { sa.rest } else { sa.get(l) };
        let refined = s.t.diff(map[&l], against);
        if s.is_empty(refined) {
            continue;
        }
        let mut m2 = map.clone();
        m2.insert(l, refined);
        rec_neg_field(s, labels, &m2, tail, label, found, lacks);
    }
}

/// The type of element `index` over every inhabited tuple in `ty`.
///
/// A tuple too short to have the element reads as `Partial`/`Absent`, never as null: a
/// different arity is a different type, not a shorter one.
pub fn project_elem(s: &mut Solver, ty: TyId, index: usize) -> Projected {
    let d = s.t.data(ty);
    let paths = s.t.tup_bdd.paths(d.tuples);
    let mut found = Vec::new();
    let mut lacks = false;
    for (pos, neg) in paths {
        tup_path_elem(s, &pos, &neg, index, &mut found, &mut lacks);
    }
    Projected::new(s, found, lacks)
}

/// One path of the tuple BDD. Arity does for tuples what labels do for records, and it
/// is coarser: a tuple atom pins its arity exactly, so positives of differing arities
/// intersect to nothing and the path contributes nothing.
///
/// Negatives of a different arity are dropped rather than walked. They cannot remove
/// anything from a path already pinned to one arity, and carrying them would branch the
/// walk over element slots they do not have.
fn tup_path_elem(
    s: &mut Solver,
    pos: &[u32],
    neg: &[u32],
    index: usize,
    found: &mut Vec<TyId>,
    lacks: &mut bool,
) {
    // No positive atom pins no arity, so every arity is in play: the element is
    // unconstrained where it exists, and absent on the arities too short for it.
    let Some(&first) = pos.first() else {
        let a = s.t.any();
        found.push(a);
        *lacks = true;
        return;
    };
    let arity = s.t.tup_atoms[first as usize].elems.len();
    for &i in pos {
        if s.t.tup_atoms[i as usize].elems.len() != arity {
            return;
        }
    }

    let any = s.t.any();
    let mut elems = vec![any; arity];
    for &i in pos {
        let a = s.t.tup_atoms[i as usize].clone();
        for (slot, ae) in elems.iter_mut().zip(&a.elems) {
            *slot = s.t.intersect(*slot, *ae);
        }
    }
    for &e in &elems {
        if s.is_empty(e) {
            return;
        }
    }

    let neg: Vec<u32> = neg
        .iter()
        .copied()
        .filter(|&j| s.t.tup_atoms[j as usize].elems.len() == arity)
        .collect();
    tup_neg_elem(s, &elems, &neg, index, found, lacks);
}

/// [`rec_neg_field`]'s counterpart for tuples: each negative branches once per element
/// slot, since a value fails to be that tuple by differing at some element. Every
/// negative here has already been filtered to the path's own arity, so the slots line up
/// and `sa.elems[k]` is always in range.
fn tup_neg_elem(
    s: &mut Solver,
    elems: &[TyId],
    neg: &[u32],
    index: usize,
    found: &mut Vec<TyId>,
    lacks: &mut bool,
) {
    let Some((&s_id, tail)) = neg.split_first() else {
        // Only an inhabited leaf gets to say the element is missing, so an arity that
        // the negatives rule out entirely does not make the read `Partial`.
        match elems.get(index) {
            Some(&e) => found.push(e),
            None => *lacks = true,
        }
        return;
    };
    let sa = s.t.tup_atoms[s_id as usize].clone();
    for k in 0..elems.len() {
        let refined = s.t.diff(elems[k], sa.elems[k]);
        if s.is_empty(refined) {
            continue;
        }
        let mut e2 = elems.to_vec();
        e2[k] = refined;
        tup_neg_elem(s, &e2, tail, index, found, lacks);
    }
}

// ---- exhaustiveness ----

/// `s ∧ ¬(⋁ covered)` — what the match does not handle.
///
/// `covered` is the *exact* arms only; see [`Test::covered`]. An inexact arm
/// contributes `never`, so a match made only of literals is never exhaustive.
///
/// The residual is the diagnostic: it names the uncovered values, so the checker can
/// say `:pending` rather than "non-exhaustive".
pub fn residual(s: &mut Solver, subject: TyId, covered: &[TyId]) -> TyId {
    let c = s.t.union_all(covered);
    s.t.diff(subject, c)
}

/// Whether [`residual`] came out empty. Kept apart from `residual` because a caller that
/// is going to report needs the residual itself — the diagnostic names the missing
/// values — and only a caller with nothing to say wants the bool.
pub fn is_exhaustive(s: &mut Solver, subject: TyId, covered: &[TyId]) -> bool {
    let r = residual(s, subject, covered);
    s.is_empty(r)
}

/// The indices of arms that can never run: either an earlier arm already took
/// everything they match, or the subject was never one of them in the first place.
///
/// Both are `arm ∧ subject ∧ ¬(⋁ earlier)` being empty, so they are one query. The
/// second is the [`Refined::NeverMatches`] case in match clothing, and reporting it is
/// what keeps an arm from being checked against a `never` binding.
pub fn redundant_arms(s: &mut Solver, subject: TyId, tests: &[Test]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut seen = s.t.never();
    for (i, test) in tests.iter().enumerate() {
        let live = {
            let reach = s.t.diff(subject, seen);
            s.t.intersect(reach, test.ty)
        };
        if s.is_empty(live) {
            out.push(i);
        }
        let c = test.covered(s);
        seen = s.t.union(seen, c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s() -> Solver {
        Solver::new()
    }

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

    #[track_caller]
    fn both(r: Refined) -> (TyId, TyId) {
        r.both().unwrap_or_else(|| panic!("expected both branches live, got {r:?}"))
    }

    #[track_caller]
    fn present(p: Projected) -> TyId {
        match p {
            Projected::Present(t) => t,
            other => panic!("expected Present, got {other:?}"),
        }
    }

    // ---- `is` ----

    #[test]
    fn is_splits_the_subject_into_both_branches() {
        let mut s = s();
        let f = s.t.f64();
        let circle = nominal(&mut s, "Circle", &[("r", f)]);
        let square = nominal(&mut s, "Square", &[("side", f)]);
        let shape = s.t.union(circle, square);

        let (then_ty, else_ty) = both(narrow_is(&mut s, shape, circle));
        assert!(s.is_equiv(then_ty, circle));
        assert!(s.is_equiv(else_ty, square), "the fallthrough names Square exactly");
    }

    #[test]
    fn narrowing_partitions_the_subject() {
        let mut s = s();
        let i = s.t.i64();
        let st = s.t.str();
        let b = s.t.bool();
        let subject = {
            let u = s.t.union(i, st);
            s.t.union(u, b)
        };
        let (then_ty, else_ty) = both(narrow_is(&mut s, subject, st));

        let meet = s.t.intersect(then_ty, else_ty);
        assert!(s.is_empty(meet), "the branches are disjoint");
        let join = s.t.union(then_ty, else_ty);
        assert!(s.is_equiv(join, subject), "and together they are the whole subject");
    }

    #[test]
    fn both_branches_stay_inside_the_subject() {
        let mut s = s();
        let i = s.t.i64();
        let n = s.t.null();
        let subject = s.t.union(i, n);
        let (then_ty, else_ty) = both(narrow_is(&mut s, subject, i));
        assert!(s.is_subtype(then_ty, subject));
        assert!(s.is_subtype(else_ty, subject));
    }

    // ---- impossible tests are reported, not silently dead ----

    #[test]
    fn a_test_on_a_disjoint_concrete_type_is_reported() {
        let mut s = s();
        let i = s.t.i64();
        let st = s.t.str();
        // `let x: i64 = 1; if x is str` — the user wants to know.
        let r = narrow_is(&mut s, i, st);
        assert_eq!(r, Refined::NeverMatches(i));
        assert_eq!(r.both(), None, "there is no then-binding to reach for");
    }

    #[test]
    fn an_is_test_on_a_type_parameter_is_reported_not_silently_dead() {
        let mut s = s();
        let t = s.t.name("T");
        let v = s.t.var(t);
        let i = s.t.i64();

        // Inside `fn f[T](x: T)`, `T` is rigid and disjoint from `i64`, so `x ∧ i64` is
        // empty. That must NOT be read as "the branch is dead, check it with a `never`
        // binding": `never <: str`, so a `g(x: str)` call inside it would typecheck
        // vacuously, and `f(5)` then hands `g` an i64 at runtime. `T` is opaque, not
        // uninhabited.
        let r = narrow_is(&mut s, v, i);
        assert_eq!(r, Refined::NeverMatches(v), "surfaced as a diagnostic");
        assert_eq!(r.both(), None, "the unsound binding is not reachable");
    }

    #[test]
    fn two_distinct_type_parameters_never_match() {
        let mut s = s();
        let t = s.t.name("T");
        let u = s.t.name("U");
        let vt = s.t.var(t);
        let vu = s.t.var(u);

        // `T ∧ U = ∅` for the same reason, and it has to surface the same way: both
        // are live once instantiated.
        let r = narrow_is(&mut s, vt, vu);
        assert_eq!(r, Refined::NeverMatches(vt));
        assert_eq!(r.both(), None);
    }

    #[test]
    fn a_test_the_subject_always_passes_is_reported_too() {
        let mut s = s();
        let i = s.t.i64();
        let st = s.t.str();
        let u = s.t.union(i, st);
        // The else-branch would bind `never`, and hide anything written in it.
        let r = narrow_is(&mut s, i, u);
        assert_eq!(r, Refined::AlwaysMatches(i));
        assert_eq!(r.both(), None);
    }

    #[test]
    fn an_empty_subject_is_unreachable_rather_than_a_bad_test() {
        let mut s = s();
        let never = s.t.never();
        let i = s.t.i64();
        assert_eq!(narrow_is(&mut s, never, i), Refined::Unreachable);
    }

    #[test]
    fn flip_preserves_which_side_is_impossible() {
        let mut s = s();
        let i = s.t.i64();
        let st = s.t.str();
        let r = narrow_is(&mut s, i, st);
        assert_eq!(r.flip(), Refined::AlwaysMatches(i), "`!(x is str)` always holds");
    }

    // ---- null ----

    #[test]
    fn not_null_recovers_the_payload_exactly() {
        let mut s = s();
        let i = s.t.i64();
        let n = s.t.null();
        let nullable = s.t.union(i, n);

        let (then_ty, else_ty) = both(narrow_not_null(&mut s, nullable));
        assert_eq!(then_ty, i, "`if p != null` gives back `i64`, not `i64|null`");
        assert_eq!(else_ty, n);
    }

    #[test]
    fn eq_null_is_the_mirror() {
        let mut s = s();
        let i = s.t.i64();
        let n = s.t.null();
        let nullable = s.t.union(i, n);

        let (then_ty, else_ty) = both(narrow_null(&mut s, nullable));
        assert_eq!(then_ty, n);
        assert_eq!(else_ty, i);
    }

    #[test]
    fn null_narrowing_does_not_leak_the_undef_marker() {
        let mut s = s();
        let i = s.t.i64();
        let n = s.t.null();
        let nullable = s.t.union(i, n);
        let undef = s.t.undef();

        // `¬null` contains B_UNDEF, so a narrowing built from the negation alone would
        // put "field absent" into the branch. Intersecting with the subject is what
        // keeps both branches in the value lattice.
        let (then_ty, _) = both(narrow_not_null(&mut s, nullable));
        let leak = s.t.intersect(then_ty, undef);
        assert!(s.is_empty(leak));
        let any = s.t.any();
        assert!(s.is_subtype(then_ty, any), "the branch is a value type");
    }

    #[test]
    fn narrowing_any_against_null_leaves_a_value_type() {
        let mut s = s();
        let any = s.t.any();
        let undef = s.t.undef();
        let (then_ty, _) = both(narrow_not_null(&mut s, any));
        let leak = s.t.intersect(then_ty, undef);
        assert!(s.is_empty(leak), "`any` is top of the value lattice, and stays there");
        assert!(s.is_subtype(then_ty, any));
    }

    #[test]
    fn a_non_nullable_subject_can_never_be_null() {
        let mut s = s();
        let i = s.t.i64();
        // `if n == null` on a bare i64 is a mistake, not a dead branch to check.
        assert_eq!(narrow_null(&mut s, i), Refined::NeverMatches(i));
    }

    #[test]
    fn null_narrowing_works_through_a_record_union() {
        let mut s = s();
        let f = s.t.f64();
        let circle = nominal(&mut s, "Circle", &[("r", f)]);
        let n = s.t.null();
        let maybe = s.t.union(circle, n);

        let (then_ty, else_ty) = both(narrow_not_null(&mut s, maybe));
        assert_eq!(then_ty, circle);
        assert_eq!(else_ty, n);
    }

    // ---- atoms and literals ----

    #[test]
    fn an_atom_pattern_refines_to_the_singleton() {
        let mut s = s();
        let ok = s.t.name("ok");
        let err = s.t.name("err");
        let a_ok = s.t.atom(ok);
        let a_err = s.t.atom(err);
        let subject = s.t.union(a_ok, a_err);

        let (then_ty, else_ty) = both(narrow_atom(&mut s, subject, ok));
        assert_eq!(then_ty, a_ok);
        assert_eq!(else_ty, a_err, "the fallthrough is `:err`, named exactly");
    }

    #[test]
    fn an_atom_pattern_the_subject_cannot_be_is_reported() {
        let mut s = s();
        let ok = s.t.name("ok");
        let nope = s.t.name("nope");
        let a_ok = s.t.atom(ok);
        assert_eq!(narrow_atom(&mut s, a_ok, nope), Refined::NeverMatches(a_ok));
    }

    #[test]
    fn a_literal_of_a_non_singleton_type_does_not_narrow_the_fallthrough() {
        let mut s = s();
        let i = s.t.i64();
        // `match n { 1 => .. }`: the arm's type is i64, but it matches one i64.
        let one = Test::inexact(i);
        let (then_ty, else_ty) = both(narrow(&mut s, i, one));
        assert!(s.is_equiv(then_ty, i));
        assert!(
            s.is_equiv(else_ty, i),
            "every i64 is still possible; only the value 1 was ruled out"
        );

        let covered = one.covered(&mut s);
        assert!(!is_exhaustive(&mut s, i, &[covered]), "literals never exhaust an i64");
    }

    #[test]
    fn a_literal_the_subject_cannot_be_is_still_reported() {
        let mut s = s();
        let f = s.t.f64();
        let circle = nominal(&mut s, "Circle", &[("r", f)]);
        let i = s.t.i64();
        // `match c { 1 => .. }` — inexactness does not excuse an impossible arm.
        assert_eq!(narrow(&mut s, circle, Test::inexact(i)), Refined::NeverMatches(circle));
    }

    #[test]
    fn null_and_atom_literals_are_exact_but_numbers_are_not() {
        let mut s = s();
        let i = s.t.i64();
        let n = s.t.null();
        let subject = s.t.union(i, n);

        // `match x { null => .., 1 => .. }` handles null but not every i64.
        let null_arm = Test::exact(n);
        let one_arm = Test::inexact(i);
        let c: Vec<TyId> = vec![null_arm.covered(&mut s), one_arm.covered(&mut s)];
        let rest = residual(&mut s, subject, &c);
        assert!(s.is_equiv(rest, i), "the residual is `i64`: the numbers are uncovered");
    }

    // ---- exhaustiveness ----

    #[test]
    fn the_residual_names_exactly_what_is_uncovered() {
        let mut s = s();
        let ok = s.t.name("ok");
        let err = s.t.name("err");
        let pending = s.t.name("pending");
        let a_ok = s.t.atom(ok);
        let a_err = s.t.atom(err);
        let a_pending = s.t.atom(pending);
        let subject = {
            let u = s.t.union(a_ok, a_err);
            s.t.union(u, a_pending)
        };

        let rest = residual(&mut s, subject, &[a_ok, a_err]);
        assert!(s.is_equiv(rest, a_pending), "the diagnostic can say `:pending`");
        assert!(!is_exhaustive(&mut s, subject, &[a_ok, a_err]));
    }

    #[test]
    fn covering_every_arm_leaves_nothing() {
        let mut s = s();
        let ok = s.t.name("ok");
        let err = s.t.name("err");
        let a_ok = s.t.atom(ok);
        let a_err = s.t.atom(err);
        let subject = s.t.union(a_ok, a_err);
        assert!(is_exhaustive(&mut s, subject, &[a_ok, a_err]));
    }

    #[test]
    fn the_residual_of_a_record_union_names_the_missing_variant() {
        let mut s = s();
        let f = s.t.f64();
        let circle = nominal(&mut s, "Circle", &[("r", f)]);
        let square = nominal(&mut s, "Square", &[("side", f)]);
        let tri = nominal(&mut s, "Tri", &[("a", f)]);
        let shape = {
            let u = s.t.union(circle, square);
            s.t.union(u, tri)
        };

        let rest = residual(&mut s, shape, &[circle, square]);
        assert!(s.is_equiv(rest, tri));
    }

    #[test]
    fn a_wildcard_exhausts_anything() {
        let mut s = s();
        let any = s.t.any();
        let f = s.t.f64();
        let circle = nominal(&mut s, "Circle", &[("r", f)]);
        assert!(is_exhaustive(&mut s, circle, &[any]));

        let i = s.t.i64();
        assert!(is_exhaustive(&mut s, i, &[any]));
        assert!(is_exhaustive(&mut s, any, &[any]));
    }

    #[test]
    fn a_nullable_match_needs_the_null_arm() {
        let mut s = s();
        let f = s.t.f64();
        let circle = nominal(&mut s, "Circle", &[("r", f)]);
        let n = s.t.null();
        let subject = s.t.union(circle, n);

        let rest = residual(&mut s, subject, &[circle]);
        assert!(s.is_equiv(rest, n), "the residual is `null` — no Option to unwrap");
        assert!(is_exhaustive(&mut s, subject, &[circle, n]));
    }

    #[test]
    fn an_empty_match_leaves_the_whole_subject() {
        let mut s = s();
        let i = s.t.i64();
        let rest = residual(&mut s, i, &[]);
        assert!(s.is_equiv(rest, i));

        let never = s.t.never();
        assert!(is_exhaustive(&mut s, never, &[]), "there is nothing to cover");
    }

    #[test]
    fn a_type_parameter_exhausts_itself_but_a_concrete_arm_does_not() {
        let mut s = s();
        let t = s.t.name("T");
        let v = s.t.var(t);
        let i = s.t.i64();
        assert!(is_exhaustive(&mut s, v, &[v]), "a generic body needs no wildcard");
        assert!(!is_exhaustive(&mut s, v, &[i]), "`is i64` covers nothing of a rigid T");
    }

    // ---- redundancy ----

    #[test]
    fn an_arm_after_a_wildcard_is_redundant() {
        let mut s = s();
        let ok = s.t.name("ok");
        let err = s.t.name("err");
        let a_ok = s.t.atom(ok);
        let a_err = s.t.atom(err);
        let subject = s.t.union(a_ok, a_err);
        let any = s.t.any();

        let arms = [Test::exact(any), Test::exact(a_ok)];
        assert_eq!(redundant_arms(&mut s, subject, &arms), vec![1]);
    }

    #[test]
    fn an_arm_disjoint_from_the_subject_is_redundant() {
        let mut s = s();
        let f = s.t.f64();
        let circle = nominal(&mut s, "Circle", &[("r", f)]);
        let square = nominal(&mut s, "Square", &[("side", f)]);

        let arms = [Test::exact(circle), Test::exact(square)];
        assert_eq!(
            redundant_arms(&mut s, circle, &arms),
            vec![1],
            "the subject was never a Square"
        );
    }

    #[test]
    fn arms_matching_a_type_parameter_against_concrete_types_are_all_redundant() {
        let mut s = s();
        let t = s.t.name("T");
        let v = s.t.var(t);
        let i = s.t.i64();
        let st = s.t.str();

        // The match form of the same hole: neither arm can run, and both are reported
        // rather than checked with a `never` binding.
        let arms = [Test::exact(i), Test::exact(st)];
        assert_eq!(redundant_arms(&mut s, v, &arms), vec![0, 1]);
    }

    #[test]
    fn distinct_arms_are_not_redundant() {
        let mut s = s();
        let ok = s.t.name("ok");
        let err = s.t.name("err");
        let a_ok = s.t.atom(ok);
        let a_err = s.t.atom(err);
        let subject = s.t.union(a_ok, a_err);

        let arms = [Test::exact(a_ok), Test::exact(a_err)];
        assert!(redundant_arms(&mut s, subject, &arms).is_empty());
    }

    #[test]
    fn a_union_arm_subsumes_a_later_member() {
        let mut s = s();
        let i = s.t.i64();
        let st = s.t.str();
        let b = s.t.bool();
        let subject = {
            let u = s.t.union(i, st);
            s.t.union(u, b)
        };
        let is_u = s.t.union(i, st);

        let arms = [Test::exact(is_u), Test::exact(i), Test::exact(b)];
        assert_eq!(redundant_arms(&mut s, subject, &arms), vec![1]);
    }

    #[test]
    fn an_inexact_arm_does_not_shadow_a_later_one() {
        let mut s = s();
        let i = s.t.i64();
        // `match n { 1 => .., _ => .. }` — the wildcard is reachable.
        let arms = [Test::inexact(i), Test::exact(i)];
        assert!(redundant_arms(&mut s, i, &arms).is_empty());
    }

    #[test]
    fn a_guard_makes_an_arm_stop_covering() {
        let mut s = s();
        let f = s.t.f64();
        let circle = nominal(&mut s, "Circle", &[("r", f)]);
        let square = nominal(&mut s, "Square", &[("side", f)]);
        let shape = s.t.union(circle, square);

        // `is Circle if r > 1 => ..` does not exhaust the Circles.
        let guarded = Test::exact(circle).guarded();
        let c: Vec<TyId> = vec![guarded.covered(&mut s), Test::exact(square).covered(&mut s)];
        let rest = residual(&mut s, shape, &c);
        assert!(s.is_equiv(rest, circle), "a guarded Circle arm leaves Circle uncovered");

        // And a later unguarded Circle arm is still reachable.
        let arms = [guarded, Test::exact(circle)];
        assert!(redundant_arms(&mut s, shape, &arms).is_empty());
    }

    // ---- record patterns ----

    #[test]
    fn a_record_pattern_narrows_a_union() {
        let mut s = s();
        let f = s.t.f64();
        let any = s.t.any();
        let circle = nominal(&mut s, "Circle", &[("r", f)]);
        let square = nominal(&mut s, "Square", &[("side", f)]);
        let shape = s.t.union(circle, square);

        // `{ r }` — only Circle has an `r`.
        let r_label = s.t.name("r");
        let pat = record_test(&mut s, None, &[(r_label, any)]);
        let (then_ty, else_ty) = both(narrow_is(&mut s, shape, pat));
        assert!(s.is_equiv(then_ty, circle));
        assert!(s.is_equiv(else_ty, square));
    }

    #[test]
    fn a_nominal_record_pattern_pins_the_tag() {
        let mut s = s();
        let f = s.t.f64();
        let any = s.t.any();
        let circle = nominal(&mut s, "Circle", &[("r", f)]);
        let ring = nominal(&mut s, "Ring", &[("r", f)]);
        let subject = s.t.union(circle, ring);

        let r_label = s.t.name("r");
        let pat = record_test(&mut s, Some(circle), &[(r_label, any)]);
        let (then_ty, else_ty) = both(narrow_is(&mut s, subject, pat));
        assert!(s.is_equiv(then_ty, circle));
        assert!(s.is_equiv(else_ty, ring), "identical shape, different name");
    }

    #[test]
    fn a_record_pattern_narrows_on_a_field_type() {
        let mut s = s();
        let i = s.t.i64();
        let st = s.t.str();
        let u = s.t.union(i, st);
        let boxed = nominal(&mut s, "Boxed", &[("v", u)]);

        // `Boxed { v: is i64 }`
        let v = s.t.name("v");
        let pat = record_test(&mut s, Some(boxed), &[(v, i)]);
        let (then_ty, else_ty) = both(narrow_is(&mut s, boxed, pat));

        assert_eq!(present(project_field(&mut s, then_ty, v)), i);
        assert_eq!(
            present(project_field(&mut s, else_ty, v)),
            st,
            "the fallthrough keeps `str`"
        );
    }

    #[test]
    fn a_structural_pattern_is_open() {
        let mut s = s();
        let st = s.t.str();
        let i = s.t.i64();
        let any = s.t.any();
        let person = nominal(&mut s, "Person", &[("name", st), ("age", i)]);

        let name = s.t.name("name");
        let pat = record_test(&mut s, None, &[(name, any)]);
        let r = narrow_is(&mut s, person, pat);
        assert_eq!(r, Refined::AlwaysMatches(person), "`{{ name }}` matches every Person");
    }

    // ---- field projection ----

    #[test]
    fn projecting_a_field_of_a_nominal_record() {
        let mut s = s();
        let f = s.t.f64();
        let circle = nominal(&mut s, "Circle", &[("r", f)]);
        let r = s.t.name("r");
        assert_eq!(project_field(&mut s, circle, r), Projected::Present(f));
    }

    #[test]
    fn projecting_a_field_across_a_union_takes_the_union() {
        let mut s = s();
        let i = s.t.i64();
        let st = s.t.str();
        let a = nominal(&mut s, "A", &[("v", i)]);
        let b = nominal(&mut s, "B", &[("v", st)]);
        let u = s.t.union(a, b);

        let v = s.t.name("v");
        let got = present(project_field(&mut s, u, v));
        let want = s.t.union(i, st);
        assert!(s.is_equiv(got, want));
    }

    #[test]
    fn projecting_a_field_only_some_variants_have_is_partial() {
        let mut s = s();
        let f = s.t.f64();
        let circle = nominal(&mut s, "Circle", &[("r", f)]);
        let square = nominal(&mut s, "Square", &[("side", f)]);
        let shape = s.t.union(circle, square);

        let r = s.t.name("r");
        assert_eq!(
            project_field(&mut s, shape, r),
            Projected::Partial(f),
            "`shape.r` is an f64 only where present — never `f64|undef` to bind"
        );

        // Narrowing first is what makes the access total.
        let (then_ty, _) = both(narrow_is(&mut s, shape, circle));
        assert_eq!(project_field(&mut s, then_ty, r), Projected::Present(f));
    }

    #[test]
    fn projecting_a_field_nothing_has_is_absent() {
        let mut s = s();
        let f = s.t.f64();
        let circle = nominal(&mut s, "Circle", &[("r", f)]);
        let missing = s.t.name("nope");
        let got = project_field(&mut s, circle, missing);
        assert_eq!(got, Projected::Absent);
        assert_eq!(got.ty(), None, "no `never` to bind and check vacuously against");
    }

    #[test]
    fn projecting_a_field_of_a_non_record_is_absent() {
        let mut s = s();
        let i = s.t.i64();
        let x = s.t.name("x");
        assert_eq!(project_field(&mut s, i, x), Projected::Absent);
    }

    #[test]
    fn projecting_a_nullable_field_and_narrowing_it() {
        let mut s = s();
        let i = s.t.i64();
        let n = s.t.null();
        let nullable = s.t.union(i, n);
        let opts = nominal(&mut s, "Opts", &[("timeout", nullable)]);

        let timeout = s.t.name("timeout");
        let got = present(project_field(&mut s, opts, timeout));
        assert_eq!(got, nullable, "present and nullable is not the same as absent");

        let (then_ty, _) = both(narrow_not_null(&mut s, got));
        assert_eq!(then_ty, i, "`opts.timeout orelse 30` is an i64");
    }

    // ---- tuple patterns ----

    #[test]
    fn a_tuple_pattern_narrows_by_arity() {
        let mut s = s();
        let i = s.t.i64();
        let pair = s.t.tuple(vec![i, i]);
        let single = s.t.tuple(vec![i]);
        let subject = s.t.union(pair, single);

        let (then_ty, else_ty) = both(narrow_is(&mut s, subject, pair));
        assert!(s.is_equiv(then_ty, pair));
        assert!(s.is_equiv(else_ty, single));
    }

    #[test]
    fn a_tuple_pattern_narrows_by_element() {
        let mut s = s();
        let i = s.t.i64();
        let st = s.t.str();
        let u = s.t.union(i, st);
        let subject = s.t.tuple(vec![u, u]);

        // `(is i64, _)`
        let any = s.t.any();
        let pat = s.t.tuple(vec![i, any]);
        let (then_ty, else_ty) = both(narrow_is(&mut s, subject, pat));

        assert_eq!(present(project_elem(&mut s, then_ty, 0)), i);
        let e1 = present(project_elem(&mut s, then_ty, 1));
        assert!(s.is_equiv(e1, u));
        assert_eq!(present(project_elem(&mut s, else_ty, 0)), st);
    }

    #[test]
    fn projecting_an_element_across_a_union() {
        let mut s = s();
        let i = s.t.i64();
        let st = s.t.str();
        let a = s.t.tuple(vec![i]);
        let b = s.t.tuple(vec![st]);
        let u = s.t.union(a, b);

        let got = present(project_elem(&mut s, u, 0));
        let want = s.t.union(i, st);
        assert!(s.is_equiv(got, want));
    }

    #[test]
    fn projecting_past_the_arity_is_absent() {
        let mut s = s();
        let i = s.t.i64();
        let single = s.t.tuple(vec![i]);
        assert_eq!(
            project_elem(&mut s, single, 1),
            Projected::Absent,
            "a 1-tuple has no second element, and no null either"
        );
    }

    #[test]
    fn projecting_an_element_only_some_arities_have_is_partial() {
        let mut s = s();
        let i = s.t.i64();
        let pair = s.t.tuple(vec![i, i]);
        let single = s.t.tuple(vec![i]);
        let subject = s.t.union(pair, single);
        assert_eq!(project_elem(&mut s, subject, 1), Projected::Partial(i));
    }

    #[test]
    fn projecting_an_element_of_a_non_tuple_is_absent() {
        let mut s = s();
        let i = s.t.i64();
        assert_eq!(project_elem(&mut s, i, 0), Projected::Absent);
    }

    #[test]
    fn tuple_arms_exhaust_a_union_of_arities() {
        let mut s = s();
        let i = s.t.i64();
        let pair = s.t.tuple(vec![i, i]);
        let single = s.t.tuple(vec![i]);
        let subject = s.t.union(pair, single);
        assert!(!is_exhaustive(&mut s, subject, &[pair]));
        assert!(is_exhaustive(&mut s, subject, &[pair, single]));

        let rest = residual(&mut s, subject, &[pair]);
        assert!(s.is_equiv(rest, single));
    }

    // ---- recursion ----

    #[test]
    fn narrowing_a_recursive_type() {
        let mut s = s();
        // `mu A = :nil | Cons { head: i64, tail: A }`
        let a = s.t.reserve();
        let i = s.t.i64();
        let cons = {
            let n = s.t.name("Cons");
            let head = s.t.name("head");
            let tail = s.t.name("tail");
            s.t.nominal(n, vec![], vec![(head, i), (tail, a)])
        };
        let nil = s.t.name("nil");
        let a_nil = s.t.atom(nil);
        let body = s.t.union(a_nil, cons);
        let d = s.t.data(body);
        s.t.define(a, d);

        let (then_ty, else_ty) = both(narrow_is(&mut s, a, cons));
        assert!(s.is_equiv(then_ty, cons));
        assert!(s.is_equiv(else_ty, a_nil));
        assert!(is_exhaustive(&mut s, a, &[cons, a_nil]));

        let tail = s.t.name("tail");
        let got = present(project_field(&mut s, then_ty, tail));
        assert!(s.is_equiv(got, a), "the tail is the recursive type again");
    }
}
