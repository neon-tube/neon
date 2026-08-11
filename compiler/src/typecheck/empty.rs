//! Emptiness, and therefore subtyping. See `docs/design/typechecker.md`.
//!
//! There is exactly one decision procedure in the checker: is this type inhabited. Every
//! question a user can ask reduces to it — `s <: t` is `s ∧ ¬t = ∅`, equivalence is
//! subtyping both ways, an exhaustive match is a leftover of `∅`. Nothing here is a
//! syntactic rule on type constructors, so there is no rule set that can disagree with
//! itself as it grows.
//!
//! A type is empty when every kind's component is. `types.rs` has already reduced each
//! component to a DNF of that kind's atoms, so the work splits three ways — records,
//! tuples, arrows — and each is decided path by path. Because `Bdd::paths` yields
//! disjoint cubes, a component is empty iff each cube is, independently.
//!
//! Each cube has the shape `⋀ᵢ Pᵢ ∧ ⋀ⱼ ¬Nⱼ`, and each kind decides it the same way: the
//! positives are meeted componentwise into one shape, and then the question is whether
//! the negatives cover it. Covering is where the exponential lives — escaping a negative
//! means differing from it on *some* component, and the choice of which one has to be
//! searched. The search is bounded in practice by how few negatives real types carry.
//!
//! Recursion is handled coinductively. `mu` types make the type graph cyclic, so a query
//! can re-enter itself; re-entry assumes emptiness and looks for a contradiction
//! elsewhere in the derivation. What makes that sound rather than wishful is that a
//! result reached under an assumption is *tainted* and never enters the memo, so an
//! answer that was only true relative to a guess can never be replayed as if it were
//! unconditional.

use super::bdd::{self, BddId};
use super::types::*;
use std::collections::{HashMap, HashSet};

/// The label standing for every field no atom on the path names. It cannot collide
/// with a real label because real labels are interned from source.
const REST: NameId = NameId(u32::MAX);

/// The type arena plus the emptiness cache over it. The two travel together because the
/// memo is only meaningful for the arena that produced the ids in it — a `TyId` means
/// nothing outside its `Types`.
pub struct Solver {
    pub t: Types,
    memo: HashMap<TyId, bool>,
    /// The queries currently in progress, i.e. the coinductive hypotheses in scope.
    assume: HashSet<TyId>,
    /// Set when a result depended on a coinductive assumption. Such a result holds
    /// only under that assumption, so it must not reach the global memo.
    tainted: bool,
}

impl Default for Solver {
    fn default() -> Self {
        Self::new()
    }
}

impl Solver {
    pub fn new() -> Self {
        Solver {
            t: Types::new(),
            memo: HashMap::new(),
            assume: HashSet::new(),
            tainted: false,
        }
    }

    /// `s <: t  ⟺  s ∧ ¬t` is empty. The whole subtype relation is this line.
    pub fn is_subtype(&mut self, a: TyId, b: TyId) -> bool {
        let d = self.t.diff(a, b);
        self.is_empty(d)
    }

    /// Semantic equality: the two denote the same set of values. This is the check to
    /// reach for, not `a == b` on the ids. Hash-consing makes equal ids imply equal types
    /// but not the reverse — `i64 | str` and `str | i64` do intern alike, yet a type
    /// reached through a `mu` unfolding or a deferred boolean op can be a second id for
    /// the same set.
    pub fn is_equiv(&mut self, a: TyId, b: TyId) -> bool {
        self.is_subtype(a, b) && self.is_subtype(b, a)
    }

    /// Is `ty` uninhabited. Everything else in this file exists to answer this.
    ///
    /// The bookkeeping around `compute` is the coinduction. On re-entry the query is
    /// assumed empty and the derivation continues; `tainted` records that the result
    /// leaned on that assumption, and only an untainted result is memoized. `outer` saves
    /// the caller's taint across the nested query, because taint is a property of a
    /// derivation, not of the solver — a sibling subquery that used no assumption must
    /// stay memoizable even while an enclosing one is tainted.
    /// `ty` with negated atoms that cannot overlap what they are subtracted from removed.
    ///
    /// A type's components are canonical individually and not jointly (`types.rs`), so a
    /// BDD keeps `¬List` on a `Map` path with no way to notice that nothing is both. That
    /// is not a wart in the abstract — it is what narrowing hands every arm of a match
    /// after the first, because each arm subtracts the ones before it:
    ///
    /// ```text
    /// match u {                            // u: List[i64] | Map[str, i64]
    ///     is List[i64] => ..,              // u: List[i64]
    ///     is Map[str, i64] => ..,          // u: Map[str, i64] & !List[i64]
    /// }
    /// ```
    ///
    /// The second arm's type is *equal* to `Map[str, i64]` and does not look like it, and
    /// the difference was load-bearing in about ten places. Every "destructure this type
    /// when it is a single atom" query in the checker — `generic::infer`'s templates,
    /// `dispatch::nominal_head`, `ordered`'s field and element readers, `as_arrow` — tests
    /// the DNF path for a single positive atom AND no negatives, so all of them declined a
    /// narrowed value. The symptoms had nothing obvious in common: `list::map` on a
    /// narrowed list reported "no impl of `Mappable`", and `<` on a narrowed record
    /// reported that the record "has no order" — a confident explanation of something
    /// untrue, which is worse than declining.
    ///
    /// Pruning here fixes the class rather than the instances. The result is not merely
    /// equivalent to the unpruned type but interns to the *same id* the user would get by
    /// writing `Map[str, i64]` directly, so every consumer sees the type they were always
    /// meant to see and the negation stops reaching diagnostics.
    ///
    /// TOP LEVEL ONLY, deliberately. The redundancy is introduced at the top by the arm
    /// subtraction, so that is where removing it pays; recursing into fields would need a
    /// cycle guard for `mu` types and would spend emptiness queries on the overwhelming
    /// majority of types that carry no negation at all. For the same reason a type whose
    /// negatives all survive is returned by id, untouched.
    pub fn prune(&mut self, ty: TyId) -> TyId {
        let d = self.t.data(ty);
        if d.records == bdd::FALSE && d.tuples == bdd::FALSE && d.arrows == bdd::FALSE {
            return ty;
        }
        let mut changed = false;
        let record = self.prune_kind(d.records, Kind::Record, &mut changed);
        let tuple = self.prune_kind(d.tuples, Kind::Tuple, &mut changed);
        let arrow = self.prune_kind(d.arrows, Kind::Arrow, &mut changed);
        if !changed {
            return ty;
        }
        // The kinds are rebuilt, so start from the type with all three cleared and union
        // them back on. A `TyData` is the union of its components, which is what lets the
        // base, atom and variable halves ride along untouched.
        let mut rest = d;
        rest.records = bdd::FALSE;
        rest.tuples = bdd::FALSE;
        rest.arrows = bdd::FALSE;
        let mut acc = self.t.intern(rest);
        for part in [record, tuple, arrow] {
            acc = self.t.union(acc, part);
        }
        acc
    }

    /// One kind's DNF, rebuilt without the negatives that subtract nothing.
    fn prune_kind(&mut self, bdd: BddId, kind: Kind, changed: &mut bool) -> TyId {
        let paths = match kind {
            Kind::Record => self.t.rec_bdd.paths(bdd),
            Kind::Tuple => self.t.tup_bdd.paths(bdd),
            Kind::Arrow => self.t.arrow_bdd.paths(bdd),
        };
        let mut acc = self.t.never();
        for (pos, neg) in paths {
            let mut shape = self.t.kind_top(kind);
            for i in &pos {
                let a = self.atom_type(*i, kind);
                shape = self.t.intersect(shape, a);
            }
            for j in &neg {
                let a = self.atom_type(*j, kind);
                // Against the shape as it stands, not as it arrived: a negative already
                // excluded by an earlier one subtracts nothing either, and dropping it is
                // as sound as dropping one that never overlapped.
                let overlap = self.t.intersect(shape, a);
                if self.is_empty(overlap) {
                    *changed = true;
                } else {
                    shape = self.t.diff(shape, a);
                }
            }
            acc = self.t.union(acc, shape);
        }
        acc
    }

    /// A type holding exactly one atom of `kind`, so it can be met against a shape.
    fn atom_type(&mut self, idx: u32, kind: Kind) -> TyId {
        match kind {
            Kind::Record => {
                let a = self.t.rec_atoms[idx as usize].clone();
                self.t.record(a)
            }
            Kind::Tuple => {
                let a = self.t.tup_atoms[idx as usize].clone();
                self.t.tuple(a.elems)
            }
            Kind::Arrow => {
                let a = self.t.arrow_atoms[idx as usize].clone();
                self.t.arrow(a.params, a.throws, a.ret)
            }
        }
    }

    /// Collapse a union of same-arity tuples in which one tuple absorbs another.
    ///
    /// `(str, i64) | (J, i64)` denotes exactly `(J, i64)` as a set of values: a tuple is
    /// covariant in its elements and `str <: J`, so `(str,i64) <: (J,i64)` and the smaller
    /// arm contributes no value the larger one lacks. The tuple BDD cannot see that — its
    /// atoms are opaque, so `A ∨ B` keeps two distinct leaves however related the shapes
    /// behind them are — so the union carries both. `ir::repr` then emits a *tagged union
    /// of two tuples*, and the coercion that widens it to the joined tuple `(J,i64)` reads
    /// whichever arm sits at tag 0 and reinterprets the other arm's bytes: a `(str,i64)`
    /// returned through the union is read back as a `(J,i64)`, a silent miscompile. Folding
    /// the union to its one maximal arm removes the union — and the lossy coercion — outright.
    ///
    /// This is absorption (`A ∨ B = B` when `A <: B`), and pointedly NOT element-wise
    /// widening. Unioning the elements position-by-position would turn `(str,i64) | (i64,str)`
    /// into `(str|i64, i64|str)`, inventing the value `(str,str)` that lives in neither arm
    /// and making `is_empty` — and therefore exhaustiveness, which is a leftover of `∅` —
    /// answer over a strictly larger set than the program can produce. Dropping only an arm
    /// *provably subsumed* by another keeps the exact set of values. It is a normalisation in
    /// the spirit of `prune`: it interns the type the user would get by writing the absorbed
    /// form directly, so every downstream consumer (`repr`, `is`, coercion) sees one tuple
    /// rather than a union that happens to be equivalent to one.
    ///
    /// Sound under intersection and negation because it touches neither: only the positive
    /// tuple leaves are rebuilt, into an equivalent BDD, and every other component of the
    /// descriptor — base, atoms, vars, records, arrows — rides along by id. Two guards make
    /// that unconditional rather than argued. `all_defined` declines while a reserved `mu`
    /// id is still a placeholder (calling `is_subtype` then would read `never` and answer
    /// wrongly; it is also the point at which a union inside a recursive body is still a
    /// deferred op). And a closing `is_equiv` against the input means that if the tuple
    /// component was ever more than a plain disjunction of positives — a negated cube left
    /// by narrowing, say — the rebuilt type is not equivalent and the input is returned
    /// untouched, so the rewrite can only ever canonicalise, never change a meaning.
    pub fn absorb_tuple_union(&mut self, ty: TyId) -> TyId {
        let d = self.t.data(ty);
        if d.tuples == bdd::FALSE || !self.t.all_defined() {
            return ty;
        }
        // The tuple atoms mentioned positively across the component. A plain union is a
        // disjunction of these; a narrowed type may carry negatives too, which the closing
        // equivalence check catches by refusing the rebuild.
        let mut atoms: Vec<u32> = self
            .t
            .tup_bdd
            .paths(d.tuples)
            .into_iter()
            .flat_map(|(pos, _)| pos)
            .collect();
        atoms.sort_unstable();
        atoms.dedup();
        if atoms.len() < 2 {
            return ty;
        }

        // Keep only the maximal atoms. `c` is dropped when a kept atom already covers it
        // (`c <: k`) — which folds equivalent atoms onto the first one seen — and displaces
        // any kept atom it strictly covers. Tuples of different arity never relate (the diff
        // subtracts nothing across arities), so mixed-arity unions keep every arm.
        let mut kept: Vec<u32> = Vec::new();
        for &c in &atoms {
            let ct = self.t.tuple(self.t.tup_atoms[c as usize].elems.clone());
            let mut covered = false;
            for &k in &kept {
                let kt = self.t.tuple(self.t.tup_atoms[k as usize].elems.clone());
                if self.is_subtype(ct, kt) {
                    covered = true;
                    break;
                }
            }
            if covered {
                continue;
            }
            let mut next: Vec<u32> = Vec::new();
            for &k in &kept {
                let kt = self.t.tuple(self.t.tup_atoms[k as usize].elems.clone());
                if !self.is_subtype(kt, ct) {
                    next.push(k);
                }
            }
            next.push(c);
            kept = next;
        }
        if kept.len() == atoms.len() {
            return ty;
        }

        // Rebuild the tuple component as the disjunction of the survivors, and splice it
        // back onto the descriptor's other, untouched components.
        let mut tup = self.t.never();
        for &k in &kept {
            let a = self.t.tuple(self.t.tup_atoms[k as usize].elems.clone());
            tup = self.t.union(tup, a);
        }
        let mut nd = d;
        nd.tuples = self.t.data(tup).tuples;
        let out = self.t.intern(nd);

        if self.is_equiv(ty, out) {
            out
        } else {
            ty
        }
    }

    pub fn is_empty(&mut self, ty: TyId) -> bool {
        // A reserved id still awaiting its body reads as `never`, so a query that
        // races resolution answers wrongly and says nothing. That is exactly how
        // `record Node { next: Node | null }` became `record Node { next: null }`.
        //
        // A hard `assert!`, not a `debug_assert!`. The check is a `HashSet::is_empty`,
        // so it is free, and the failure it guards against is a *silent wrong answer* in
        // the one decision procedure the whole checker rests on. Leaving it to debug
        // builds would mean the tests are loud and the shipped compiler is quiet, which
        // is the wrong way round.
        assert!(
            self.t.all_defined(),
            "is_empty ran while a reserved id was still undefined"
        );
        if let Some(&r) = self.memo.get(&ty) {
            return r;
        }
        // Re-entering a query already in progress: assume it, and look for a
        // contradiction in the rest of the derivation. Contractivity (checked at the
        // declaration, not here) is what guarantees this terminates.
        if self.assume.contains(&ty) {
            self.tainted = true;
            return true;
        }

        self.assume.insert(ty);
        let outer = self.tainted;
        self.tainted = false;

        let r = self.compute(ty);

        let used_assumption = self.tainted;
        self.tainted = outer || used_assumption;
        self.assume.remove(&ty);

        if !used_assumption {
            self.memo.insert(ty, r);
        }
        r
    }

    /// Emptiness with no caching or cycle handling: every component must be empty.
    ///
    /// The components are tested cheapest-first, and each is a bail-out. Base bits and
    /// the two atom sets are O(1); the three kinds below them each enumerate DNF paths
    /// and can recurse, so a type that is obviously inhabited never pays for them.
    ///
    /// `B_UNDEF` counts as inhabited here. It is not a value a user can write, but record
    /// decomposition needs "absent" to be an ordinary member of the field lattice, and a
    /// field that can only be absent is a real, satisfiable constraint.
    fn compute(&mut self, ty: TyId) -> bool {
        let d = self.t.data(ty);
        if d.base != 0 {
            return false;
        }
        if !self.t.atomset_of(d.atoms).is_empty_set() {
            return false;
        }
        if !self.t.atomset_of(d.vars).is_empty_set() {
            return false;
        }
        if !self.records_empty(d.records) {
            return false;
        }
        if !self.tuples_empty(d.tuples) {
            return false;
        }
        if !self.arrows_empty(d.arrows) {
            return false;
        }
        true
    }

    // ---- records ----

    /// The record component is empty iff every DNF cube is. The cubes are disjoint, so
    /// one inhabited cube inhabits the whole component and the rest need not be looked at.
    fn records_empty(&mut self, b: BddId) -> bool {
        for (pos, neg) in self.t.rec_bdd.paths(b) {
            if !self.rec_path_empty(&pos, &neg) {
                return false;
            }
        }
        true
    }

    /// `⋀ᵢ Rᵢ ∧ ⋀ⱼ ¬Sⱼ`, decomposed field-wise.
    ///
    /// The label set is every label *mentioned* by any atom on the path, positive or
    /// negative; everything else is uniform and travels as `rest`. Each label starts at
    /// the top of the field lattice — `any_or_undef`, not `any`, since a label one atom
    /// names may be legitimately absent in another — and the positives are meeted into
    /// it. `RecordAtom::get` returning `rest` for a label it does not name is what makes
    /// that meet total, so no atom needs to be widened to a common label set first.
    ///
    /// If any field, or `rest` itself, comes out empty, the cube is empty and the
    /// negatives are irrelevant. This single check is where nominal disjointness is
    /// decided: `Red ∧ Green` meets `:Red` with `:Green` in the `#nominal` field and
    /// gets an empty atom set, with no rule about nominal types written anywhere.
    ///
    /// `rest` is then handed to `rec_neg` as a pseudo-field under `REST`, because a
    /// record can also escape a negative on a label neither of them names.
    fn rec_path_empty(&mut self, pos: &[u32], neg: &[u32]) -> bool {
        let mut labels: Vec<NameId> = Vec::new();
        for &i in pos.iter().chain(neg) {
            labels.extend(self.t.rec_atoms[i as usize].labels());
        }
        labels.sort_unstable();
        labels.dedup();

        let full = self.t.any_or_undef();
        let mut map: HashMap<NameId, TyId> = labels.iter().map(|&l| (l, full)).collect();
        let mut rest = full;

        for &i in pos {
            let a = self.t.rec_atoms[i as usize].clone();
            for &l in &labels {
                let g = a.get(l);
                let cur = map[&l];
                let n = self.t.intersect(cur, g);
                map.insert(l, n);
            }
            rest = self.t.intersect(rest, a.rest);
        }

        for &l in &labels {
            if self.field_empty(l, map[&l]) {
                return true;
            }
        }
        if self.is_empty(rest) {
            return true;
        }

        map.insert(REST, rest);
        self.rec_neg(&labels, &map, neg)
    }

    /// A field's emptiness as it counts against its atom's inhabitedness.
    ///
    /// A type-argument slot (`#0`, `#1`, …) is identity, not data: no value of
    /// `List[Tree]` contains a `Tree`-typed member for the slot — the empty list
    /// inhabits `List[Tree]` whatever `Tree` is. So a slot kills its atom only when it
    /// is empty *without leaning on the in-progress assumption*: `List[i64] ∧
    /// List[str]`'s slot is `i64 ∧ str`, a closed `never`, and nominal argument
    /// disjointness lives exactly there — while `record Tree { kids: List[Tree] }`
    /// reaches the slot only through its own cycle, and the assumption-tainted "empty"
    /// that comes back is the inductive reading of a phantom position, which made every
    /// record whose recursion runs through a container falsely uninhabited (`Tree` had
    /// "no field `v`", because no projection path survived).
    ///
    /// Data fields stay inductive, deliberately: a record cycle with no base case
    /// (`record Wrap { t: Tree }  record Tree { kids: Wrap }`) has no finite values,
    /// and reporting it empty is correct.
    pub fn field_empty(&mut self, l: NameId, ty: TyId) -> bool {
        if !self.arg_slot(l) {
            return self.is_empty(ty);
        }
        let outer = self.tainted;
        self.tainted = false;
        let r = self.is_empty(ty);
        let leaned = self.tainted;
        self.tainted = outer || leaned;
        r && !leaned
    }

    /// Whether `l` is a nominal type-argument slot: `#` followed by digits. `#nominal`
    /// and `#inner` do not match — the tag is an atom and the newtype payload is data.
    fn arg_slot(&self, l: NameId) -> bool {
        let n = self.t.name_str(l);
        n.len() > 1 && n.starts_with('#') && n[1..].bytes().all(|b| b.is_ascii_digit())
    }

    /// To escape every negative, the record must differ from each on *some* field.
    /// Search the choice of witness field per negative; exponential in the number of
    /// negatives, which in real programs is one or two.
    ///
    /// Returns whether the cube is *empty*, so the base case — no negatives left — is
    /// `false`: the positives were already shown inhabited, and nothing is subtracting
    /// from them any more.
    ///
    /// The refinement is threaded into the recursive call rather than being decided per
    /// negative in isolation. That matters: a witness field chosen to escape `S₁` shrinks
    /// what is available to escape `S₂`, and evaluating the negatives independently would
    /// accept a record no single value can actually be. A witness whose refinement is
    /// empty is not a choice at all and is skipped, not failed.
    fn rec_neg(&mut self, labels: &[NameId], map: &HashMap<NameId, TyId>, neg: &[u32]) -> bool {
        let Some((&s_id, tail)) = neg.split_first() else {
            return false;
        };
        let s = self.t.rec_atoms[s_id as usize].clone();

        for &l in labels.iter().chain(std::iter::once(&REST)) {
            let against = if l == REST { s.rest } else { s.get(l) };
            let cur = map[&l];
            let refined = self.t.diff(cur, against);
            if self.is_empty(refined) {
                continue;
            }
            let mut m2 = map.clone();
            m2.insert(l, refined);
            if !self.rec_neg(labels, &m2, tail) {
                return false;
            }
        }
        true
    }

    // ---- tuples ----

    /// As `records_empty`, over the tuple arena.
    fn tuples_empty(&mut self, b: BddId) -> bool {
        for (pos, neg) in self.t.tup_bdd.paths(b) {
            if !self.tup_path_empty(&pos, &neg) {
                return false;
            }
        }
        true
    }

    /// The record decomposition, specialised to a fixed arity.
    ///
    /// Arity does the work labels do for records: positives that disagree on it are
    /// immediately empty, and negatives of a different arity are dropped, since a tuple
    /// can never be one of them and they subtract nothing. Slots start at `any` rather
    /// than `any_or_undef` — a tuple has no notion of an absent element, so `undef` has
    /// no place in the element lattice.
    fn tup_path_empty(&mut self, pos: &[u32], neg: &[u32]) -> bool {
        // Arities are infinite and tuples of different arity are disjoint, so with no
        // positive atom the negatives can never cover everything.
        if pos.is_empty() {
            return false;
        }
        let arity = self.t.tup_atoms[pos[0] as usize].elems.len();
        for &i in pos {
            if self.t.tup_atoms[i as usize].elems.len() != arity {
                return true;
            }
        }

        let any = self.t.any();
        let mut elems = vec![any; arity];
        for &i in pos {
            let a = self.t.tup_atoms[i as usize].clone();
            for (slot, ae) in elems.iter_mut().zip(&a.elems) {
                *slot = self.t.intersect(*slot, *ae);
            }
        }
        for &e in &elems {
            if self.is_empty(e) {
                return true;
            }
        }

        let neg: Vec<u32> = neg
            .iter()
            .copied()
            .filter(|&j| self.t.tup_atoms[j as usize].elems.len() == arity)
            .collect();
        self.tup_neg(&elems, &neg)
    }

    /// `rec_neg` for tuples: escape each negative on some slot, threading the refinement
    /// through. Every atom here has the same arity — `tup_path_empty` filtered the rest —
    /// so indexing `s.elems[k]` is in bounds by construction, and there is no `REST`
    /// pseudo-slot to consider.
    fn tup_neg(&mut self, elems: &[TyId], neg: &[u32]) -> bool {
        let Some((&s_id, tail)) = neg.split_first() else {
            return false;
        };
        let s = self.t.tup_atoms[s_id as usize].clone();
        for k in 0..elems.len() {
            let refined = self.t.diff(elems[k], s.elems[k]);
            if self.is_empty(refined) {
                continue;
            }
            let mut e2 = elems.to_vec();
            e2[k] = refined;
            if !self.tup_neg(&e2, tail) {
                return false;
            }
        }
        true
    }

    // ---- arrows ----

    /// As `records_empty`, over the arrow arena.
    fn arrows_empty(&mut self, b: BddId) -> bool {
        for (pos, neg) in self.t.arrow_bdd.paths(b) {
            if !self.arrow_path_empty(&pos, &neg) {
                return false;
            }
        }
        true
    }

    /// Arrows do not decompose the way records and tuples do, and this is the one place
    /// the shape of the reasoning changes.
    ///
    /// `⋀P ∧ ⋀ⱼ¬Sⱼ` is empty exactly when some *single* `Sⱼ` already contains all of `P`
    /// — there is no combining of negatives, hence no witness search here. The
    /// combinatorial cost moves inside `arrow_le` instead, where it is over the
    /// positives.
    ///
    /// With no positives the conjunction is the whole arrow kind, and with no negatives
    /// nothing subtracts from it, so an empty `neg` is inhabited outright. A negative of
    /// a different arity is skipped rather than failing the cube: no function has two
    /// arities, so such an `S` contains nothing the positives could be and subtracts
    /// nothing.
    fn arrow_path_empty(&mut self, pos: &[u32], neg: &[u32]) -> bool {
        // Arity first: no function has two of them, so positives that disagree are
        // empty regardless of what the negatives say.
        let mut arity = None;
        if let Some(&first) = pos.first() {
            let a0 = self.t.arrow_atoms[first as usize].params.len();
            for &i in pos {
                if self.t.arrow_atoms[i as usize].params.len() != a0 {
                    return true;
                }
            }
            arity = Some(a0);
        }
        if neg.is_empty() {
            return false;
        }
        for &j in neg {
            let sj = self.t.arrow_atoms[j as usize].clone();
            if arity.is_some_and(|a| sj.params.len() != a) {
                continue;
            }
            if self.arrow_le(pos, &sj) {
                return true;
            }
        }
        false
    }

    /// `⋀_{i∈P}(sᵢ→tᵢ) ≤ (s→t)`
    ///   iff `∀P'⊆P: s ≤ ⋁_{i∈P'} sᵢ  or  ⋀_{i∈P∖P'} tᵢ ≤ t`
    ///
    /// Frisch, Castagna, Benzaken, *Semantic subtyping*, JACM 2008, §4.
    /// Exponential in |P| — the count of positive arrows in one intersection.
    ///
    /// The codomain is a *sum*: a call returns or it throws. So it decomposes into
    /// two independent components, each carrying its own intersection — never a
    /// tuple of the two. A tuple is empty as soon as one side is, and `never` is a
    /// subtype of everything, so every non-throwing function would pass the
    /// codomain check regardless of its return.
    ///
    /// `mask` enumerates the subsets `P'`: bit `k` set puts the k-th positive on the
    /// domain side of the disjunction, clear puts it on the codomain side. Domains are
    /// compared as tuples so that multi-parameter subtyping reuses the tuple
    /// decomposition rather than open-coding a componentwise rule that could drift from
    /// it.
    fn arrow_le(&mut self, pos: &[u32], s: &ArrowAtom) -> bool {
        let s_dom = self.t.tuple(s.params.clone());
        let n = pos.len();
        // `1u32 << n` is undefined for n >= 32: a debug build panics, and a release build
        // wraps — `1u32 << 32` is `1`, so the loop would run the single mask 0 and could
        // return `true` (covered) having examined one of four billion subsets. That is a
        // wrong subtyping answer with no diagnostic. Refuse instead. Nothing gets here in
        // practice: 2^31 masks would not terminate anyway, so an intersection of 32
        // function types is already outside what this can decide, and saying so is the
        // only honest option.
        assert!(
            n < 32,
            "arrow_le: {n} positive arrows in one intersection is beyond this decision \
             procedure (the subset search is 2^n)"
        );
        for mask in 0..(1u32 << n) {
            let mut dom_union = self.t.never();
            let mut ret_inter = self.t.any();
            let mut throws_inter = self.t.any();
            for (k, &i) in pos.iter().enumerate() {
                let a = self.t.arrow_atoms[i as usize].clone();
                if mask >> k & 1 == 1 {
                    let d = self.t.tuple(a.params);
                    dom_union = self.t.union(dom_union, d);
                } else {
                    ret_inter = self.t.intersect(ret_inter, a.ret);
                    throws_inter = self.t.intersect(throws_inter, a.throws);
                }
            }
            if self.is_subtype(s_dom, dom_union) {
                continue;
            }
            if self.is_subtype(ret_inter, s.ret) && self.is_subtype(throws_inter, s.throws) {
                continue;
            }
            return false;
        }
        true
    }
}
