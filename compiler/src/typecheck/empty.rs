use super::bdd::BddId;
use super::types::*;
use std::collections::{HashMap, HashSet};

/// The label standing for every field no atom on the path names. It cannot collide
/// with a real label because real labels are interned from source.
const REST: NameId = NameId(u32::MAX);

pub struct Solver {
    pub t: Types,
    memo: HashMap<TyId, bool>,
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

    pub fn is_equiv(&mut self, a: TyId, b: TyId) -> bool {
        self.is_subtype(a, b) && self.is_subtype(b, a)
    }

    pub fn is_empty(&mut self, ty: TyId) -> bool {
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

    fn compute(&mut self, ty: TyId) -> bool {
        let d = self.t.data(ty);
        if d.base != 0 {
            return false;
        }
        if !self.t.atomset_of(d.atoms).is_empty_set() {
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

    fn records_empty(&mut self, b: BddId) -> bool {
        for (pos, neg) in self.t.rec_bdd.paths(b) {
            if !self.rec_path_empty(&pos, &neg) {
                return false;
            }
        }
        true
    }

    /// `⋀ᵢ Rᵢ ∧ ⋀ⱼ ¬Sⱼ`, decomposed field-wise.
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
            if self.is_empty(map[&l]) {
                return true;
            }
        }
        if self.is_empty(rest) {
            return true;
        }

        map.insert(REST, rest);
        self.rec_neg(&labels, &map, neg)
    }

    /// To escape every negative, the record must differ from each on *some* field.
    /// Search the choice of witness field per negative; exponential in the number of
    /// negatives, which in real programs is one or two.
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

    fn tuples_empty(&mut self, b: BddId) -> bool {
        for (pos, neg) in self.t.tup_bdd.paths(b) {
            if !self.tup_path_empty(&pos, &neg) {
                return false;
            }
        }
        true
    }

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

    fn arrows_empty(&mut self, b: BddId) -> bool {
        for (pos, neg) in self.t.arrow_bdd.paths(b) {
            if !self.arrow_path_empty(&pos, &neg) {
                return false;
            }
        }
        true
    }

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
    fn arrow_le(&mut self, pos: &[u32], s: &ArrowAtom) -> bool {
        let s_dom = self.t.tuple(s.params.clone());
        let n = pos.len();
        for mask in 0..(1u32 << n) {
            let mut dom_union = self.t.never();
            let mut ret_inter = self.t.any();
            for (k, &i) in pos.iter().enumerate() {
                let a = self.t.arrow_atoms[i as usize].clone();
                if mask >> k & 1 == 1 {
                    let d = self.t.tuple(a.params);
                    dom_union = self.t.union(dom_union, d);
                } else {
                    ret_inter = self.t.intersect(ret_inter, a.ret);
                }
            }
            if self.is_subtype(s_dom, dom_union) {
                continue;
            }
            if self.is_subtype(ret_inter, s.ret) {
                continue;
            }
            return false;
        }
        true
    }
}
