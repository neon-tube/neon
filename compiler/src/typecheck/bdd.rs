//! Reduced ordered binary decision diagrams, one arena per type kind.
//!
//! A kind's component of a type (its records, its tuples, its arrows) is an arbitrary
//! boolean combination of that kind's atoms, and this is where those combinations live.
//! The arena keeps them in canonical form by two rules applied in `node`: a decision
//! whose branches agree is dropped, and every surviving node is interned. Together those
//! make `BddId` equality decide *logical* equivalence — `(x ∧ y)` and `(y ∧ x)` are one
//! id — which is what lets `TyData` be `Copy`, hashable, and compared with `==`.
//!
//! Atom ids double as the variable order, so an arena's order is fixed by the order its
//! caller interned atoms in. Nothing here reorders; correctness does not depend on the
//! order being good, only on it being the same for every diagram in the arena, which
//! holds because `Types` owns one arena per kind and hands out atom ids from one counter.
//!
//! The operations are memoized per arena. That is only sound because a kind's diagrams
//! never mention another kind's atoms — an `i64` is never a record, so no kind's meaning
//! leaks into another's, and a `(BddId, BddId)` key is a complete description of the
//! query.
//!
//! Emptiness is deliberately not decided here. This layer is pure boolean algebra over
//! opaque variables; `paths` hands the DNF to `empty.rs`, which knows what the atoms of
//! each kind actually mean.

use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct BddId(u32);

pub const FALSE: BddId = BddId(0);
pub const TRUE: BddId = BddId(1);

/// A decision on one atom: `high` is the diagram when the atom holds, `low` when it does
/// not. Both branches point at strictly higher atoms or at a terminal, which is the
/// ordering invariant every operation below relies on.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Node {
    atom: u32,
    high: BddId,
    low: BddId,
}

/// A BDD over one kind's atoms. Atom ids are the variable order.
#[derive(Default)]
pub struct Bdd {
    nodes: Vec<Node>,
    interned: HashMap<Node, BddId>,
    and_memo: HashMap<(BddId, BddId), BddId>,
    or_memo: HashMap<(BddId, BddId), BddId>,
    not_memo: HashMap<BddId, BddId>,
}

impl Bdd {
    pub fn new() -> Self {
        // 0 and 1 are the terminals; the entries are never read. Every site that indexes
        // `nodes` rules terminals out first. `u32::MAX` is chosen anyway so that a
        // terminal could never win the `min` that picks the next atom to split on.
        let dummy = Node {
            atom: u32::MAX,
            high: FALSE,
            low: FALSE,
        };
        Bdd {
            nodes: vec![dummy, dummy],
            ..Default::default()
        }
    }

    /// The single place a node is created, and therefore the single place canonicity is
    /// enforced: a decision whose branches agree is not a decision, and an identical node
    /// is never allocated twice. Skip this and build a `Node` by hand and `BddId`
    /// equality stops meaning logical equality, which every other file assumes it does.
    fn node(&mut self, atom: u32, high: BddId, low: BddId) -> BddId {
        if high == low {
            return high;
        }
        let n = Node { atom, high, low };
        if let Some(&id) = self.interned.get(&n) {
            return id;
        }
        let id = BddId(self.nodes.len() as u32);
        self.nodes.push(n);
        self.interned.insert(n, id);
        id
    }

    /// The diagram for a bare atom. `atom` is an index into the owning kind's atom table
    /// (`rec_atoms`, `tup_atoms`, `arrow_atoms`); this arena never learns what it means.
    pub fn atom(&mut self, atom: u32) -> BddId {
        self.node(atom, TRUE, FALSE)
    }

    fn is_terminal(b: BddId) -> bool {
        b == FALSE || b == TRUE
    }

    fn top(&self, b: BddId) -> u32 {
        self.nodes[b.0 as usize].atom
    }

    /// (high, low) cofactors of `b` with respect to `atom`.
    ///
    /// A diagram that does not decide on `atom` at its root is independent of it, so both
    /// cofactors are `b` itself. That case is what lets `and`/`or` descend two diagrams
    /// with different roots in lockstep: they split on the smaller of the two atoms, and
    /// the one that has not reached it yet simply passes through unchanged.
    fn split(&self, b: BddId, atom: u32) -> (BddId, BddId) {
        if Self::is_terminal(b) || self.top(b) != atom {
            (b, b)
        } else {
            let n = self.nodes[b.0 as usize];
            (n.high, n.low)
        }
    }

    /// Conjunction, by the standard Shannon recursion: split both operands on the
    /// smallest root atom, recurse on the two cofactor pairs, rebuild through `node`.
    ///
    /// The terminal cases above the memo lookup are not just a fast path — they are what
    /// guarantees both operands are real nodes by the time `top` indexes `nodes`. The
    /// memo key is sorted because conjunction is commutative, which halves the table and
    /// makes `a ∧ b` hit the entry left by `b ∧ a`.
    pub fn and(&mut self, a: BddId, b: BddId) -> BddId {
        if a == FALSE || b == FALSE {
            return FALSE;
        }
        if a == TRUE {
            return b;
        }
        if b == TRUE || a == b {
            return a;
        }
        let key = if a <= b { (a, b) } else { (b, a) };
        if let Some(&r) = self.and_memo.get(&key) {
            return r;
        }
        let atom = self.top(a).min(self.top(b));
        let (ah, al) = self.split(a, atom);
        let (bh, bl) = self.split(b, atom);
        let high = self.and(ah, bh);
        let low = self.and(al, bl);
        let r = self.node(atom, high, low);
        self.and_memo.insert(key, r);
        r
    }

    /// Disjunction, the dual of `and` and structured identically. It keeps its own memo:
    /// the two tables must not be shared, since they answer different questions about the
    /// same pair.
    pub fn or(&mut self, a: BddId, b: BddId) -> BddId {
        if a == TRUE || b == TRUE {
            return TRUE;
        }
        if a == FALSE {
            return b;
        }
        if b == FALSE || a == b {
            return a;
        }
        let key = if a <= b { (a, b) } else { (b, a) };
        if let Some(&r) = self.or_memo.get(&key) {
            return r;
        }
        let atom = self.top(a).min(self.top(b));
        let (ah, al) = self.split(a, atom);
        let (bh, bl) = self.split(b, atom);
        let high = self.or(ah, bh);
        let low = self.or(al, bl);
        let r = self.node(atom, high, low);
        self.or_memo.insert(key, r);
        r
    }

    /// Complement, by swapping the terminals underneath every decision.
    ///
    /// This walks and rebuilds the whole diagram rather than using complement edges — an
    /// id here always denotes a positive diagram, so nothing downstream has to know about
    /// a sign bit riding on a `BddId`. The memo keeps the cost linear in nodes visited.
    pub fn not(&mut self, a: BddId) -> BddId {
        match a {
            FALSE => return TRUE,
            TRUE => return FALSE,
            _ => {}
        }
        if let Some(&r) = self.not_memo.get(&a) {
            return r;
        }
        let n = self.nodes[a.0 as usize];
        let high = self.not(n.high);
        let low = self.not(n.low);
        let r = self.node(n.atom, high, low);
        self.not_memo.insert(a, r);
        r
    }

    /// `a ∖ b`, i.e. `a ∧ ¬b`. Set difference is the operation subtyping is phrased in,
    /// so it gets a name here rather than being spelled out at every call site.
    pub fn diff(&mut self, a: BddId, b: BddId) -> BddId {
        let nb = self.not(b);
        self.and(a, nb)
    }

    /// Enumerate the DNF paths to TRUE as (positive atoms, negative atoms).
    ///
    /// Emptiness of a kind is decided per path by the caller, because whether a path
    /// is satisfiable depends on that kind's atom semantics — which is the whole
    /// reason the kinds are separated.
    ///
    /// The cubes are pairwise disjoint: any two paths part at some node, so one takes the
    /// atom positively and the other negatively. That is why the caller may decide each
    /// path on its own and answer "empty" only if every one of them is — with overlapping
    /// cubes that reasoning would not be available.
    ///
    /// Worst case this is exponential in the number of atoms, and nothing here caps it.
    /// It stays tractable because a real program's type mentions a handful of record or
    /// arrow shapes, not hundreds.
    pub fn paths(&self, b: BddId) -> Vec<(Vec<u32>, Vec<u32>)> {
        let mut out = Vec::new();
        let mut pos = Vec::new();
        let mut neg = Vec::new();
        self.walk(b, &mut pos, &mut neg, &mut out);
        out
    }

    /// Depth-first accumulation for `paths`. `pos`/`neg` are the atoms decided so far on
    /// the way down and are restored on the way back up, so they describe exactly the
    /// current branch; a cube is cloned out only where the branch reaches TRUE. Branches
    /// reaching FALSE contribute nothing, which is how the reduced diagram prunes.
    fn walk(
        &self,
        b: BddId,
        pos: &mut Vec<u32>,
        neg: &mut Vec<u32>,
        out: &mut Vec<(Vec<u32>, Vec<u32>)>,
    ) {
        match b {
            FALSE => {}
            TRUE => out.push((pos.clone(), neg.clone())),
            _ => {
                let n = self.nodes[b.0 as usize];
                pos.push(n.atom);
                self.walk(n.high, pos, neg, out);
                pos.pop();
                neg.push(n.atom);
                self.walk(n.low, pos, neg, out);
                neg.pop();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminals() {
        let mut b = Bdd::new();
        assert_eq!(b.not(TRUE), FALSE);
        assert_eq!(b.not(FALSE), TRUE);
        assert_eq!(b.and(TRUE, FALSE), FALSE);
        assert_eq!(b.or(TRUE, FALSE), TRUE);
    }

    #[test]
    fn complement_is_empty_and_total() {
        let mut b = Bdd::new();
        let x = b.atom(0);
        let nx = b.not(x);
        assert_eq!(b.and(x, nx), FALSE);
        assert_eq!(b.or(x, nx), TRUE);
    }

    #[test]
    fn interning_makes_equal_terms_identical() {
        let mut b = Bdd::new();
        let x = b.atom(0);
        let y = b.atom(1);
        let a1 = b.and(x, y);
        let a2 = b.and(y, x);
        assert_eq!(a1, a2);
    }

    #[test]
    fn de_morgan() {
        let mut b = Bdd::new();
        let x = b.atom(0);
        let y = b.atom(1);
        let lhs = {
            let t = b.and(x, y);
            b.not(t)
        };
        let rhs = {
            let nx = b.not(x);
            let ny = b.not(y);
            b.or(nx, ny)
        };
        assert_eq!(lhs, rhs);
    }

    #[test]
    fn absorption() {
        let mut b = Bdd::new();
        let x = b.atom(0);
        let y = b.atom(1);
        let xy = b.and(x, y);
        assert_eq!(b.or(x, xy), x);
    }

    #[test]
    fn paths_of_single_atom() {
        let mut b = Bdd::new();
        let x = b.atom(3);
        let p = b.paths(x);
        assert_eq!(p, vec![(vec![3], vec![])]);
    }

    #[test]
    fn paths_of_difference() {
        let mut b = Bdd::new();
        let x = b.atom(0);
        let y = b.atom(1);
        let d = b.diff(x, y);
        assert_eq!(d, {
            let ny = b.not(y);
            b.and(x, ny)
        });
        assert_eq!(b.paths(d), vec![(vec![0], vec![1])]);
    }
}
