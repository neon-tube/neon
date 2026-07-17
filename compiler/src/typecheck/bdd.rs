use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct BddId(u32);

pub const FALSE: BddId = BddId(0);
pub const TRUE: BddId = BddId(1);

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
        // 0 and 1 are the terminals; the entries are never read.
        let dummy = Node { atom: u32::MAX, high: FALSE, low: FALSE };
        Bdd { nodes: vec![dummy, dummy], ..Default::default() }
    }

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
    fn split(&self, b: BddId, atom: u32) -> (BddId, BddId) {
        if Self::is_terminal(b) || self.top(b) != atom {
            (b, b)
        } else {
            let n = self.nodes[b.0 as usize];
            (n.high, n.low)
        }
    }

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

    pub fn diff(&mut self, a: BddId, b: BddId) -> BddId {
        let nb = self.not(b);
        self.and(a, nb)
    }

    /// Enumerate the DNF paths to TRUE as (positive atoms, negative atoms).
    ///
    /// Emptiness of a kind is decided per path by the caller, because whether a path
    /// is satisfiable depends on that kind's atom semantics — which is the whole
    /// reason the kinds are separated.
    pub fn paths(&self, b: BddId) -> Vec<(Vec<u32>, Vec<u32>)> {
        let mut out = Vec::new();
        let mut pos = Vec::new();
        let mut neg = Vec::new();
        self.walk(b, &mut pos, &mut neg, &mut out);
        out
    }

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
