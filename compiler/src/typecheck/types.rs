use super::bdd::{self, Bdd, BddId};
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct NameId(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct TyId(pub u32);

// ---- base primitives ----

pub type BaseSet = u8;

pub const B_I64: BaseSet = 1 << 0;
pub const B_F64: BaseSet = 1 << 1;
pub const B_STR: BaseSet = 1 << 2;
pub const B_BOOL: BaseSet = 1 << 3;
pub const B_NULL: BaseSet = 1 << 4;

/// Not a value, and unwritable in source: it marks a record field as absent.
/// Field-wise record decomposition needs a total map from label to type, so
/// "not present" has to be a member of the field lattice.
pub const B_UNDEF: BaseSet = 1 << 5;

pub const B_ANY: BaseSet = B_I64 | B_F64 | B_STR | B_BOOL | B_NULL;
pub const B_ALL: BaseSet = B_ANY | B_UNDEF;

// ---- atoms (`:ok`) ----

/// Atom names are countably infinite but any one type mentions finitely many, so
/// a set of them is finite or cofinite.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct AtomSet {
    pub neg: bool,
    pub names: Vec<NameId>,
}

impl AtomSet {
    fn empty() -> Self {
        AtomSet { neg: false, names: vec![] }
    }
    fn all() -> Self {
        AtomSet { neg: true, names: vec![] }
    }
    fn is_empty(&self) -> bool {
        !self.neg && self.names.is_empty()
    }
    fn contains(&self, n: NameId) -> bool {
        self.names.binary_search(&n).is_ok() != self.neg
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct AtomSetId(pub u32);

/// A boolean op whose operands were not all defined when it was written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PendingOp {
    Union(TyId, TyId),
    Intersect(TyId, TyId),
    Negate(TyId),
}

// ---- kind atoms ----

/// A record shape as a total map from label to type: explicit `fields`, and
/// `rest` for every label not named.
///
/// Nominal identity rides along as the reserved `#nominal` field holding an atom
/// singleton, and generic arguments as `#0`, `#1`, .... This is not a trick for its
/// own sake: it makes nominal disjointness (`:Red ∧ :Green = ∅`), nominal-satisfies-
/// structural, and generic covariance all fall out of the same field-wise
/// decomposition, instead of being three rules that have to agree with each other.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct RecordAtom {
    /// sorted by label
    pub fields: Vec<(NameId, TyId)>,
    pub rest: TyId,
}

impl RecordAtom {
    pub fn get(&self, label: NameId) -> TyId {
        match self.fields.binary_search_by_key(&label, |f| f.0) {
            Ok(i) => self.fields[i].1,
            Err(_) => self.rest,
        }
    }
    pub fn labels(&self) -> impl Iterator<Item = NameId> + '_ {
        self.fields.iter().map(|f| f.0)
    }
}

/// Tuples of different arity are disjoint, so arity is part of the atom's identity
/// and never needs a rule of its own.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct TupleAtom {
    pub elems: Vec<TyId>,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ArrowAtom {
    pub params: Vec<TyId>,
    /// What the function may throw. A function that throws nothing has `never`
    /// here, so an absent `throws` clause must resolve to `never` and never `any`.
    pub throws: TyId,
    pub ret: TyId,
}

// ---- the descriptor ----

/// One field per kind, each a BDD over only that kind's atoms.
///
/// Boolean ops are field-wise; emptiness is every-field-empty, decided per kind.
/// An `i64` is never a record, so no kind's emptiness can depend on another's, and
/// nothing has to carry a path. That is what makes the memo tables sound.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TyData {
    pub base: BaseSet,
    pub atoms: AtomSetId,
    /// Rigid type variables. Inside `fn show[T](x: T)`, `T` is opaque: a singleton
    /// disjoint from every other type, exactly like an atom, so it reuses the
    /// finite-or-cofinite machinery rather than needing a sixth BDD.
    pub vars: AtomSetId,
    pub records: BddId,
    pub tuples: BddId,
    pub arrows: BddId,
}

pub struct Types {
    names: Vec<String>,
    name_map: HashMap<String, NameId>,

    tys: Vec<TyData>,
    ty_map: HashMap<TyData, TyId>,

    atomsets: Vec<AtomSet>,
    atomset_map: HashMap<AtomSet, AtomSetId>,

    pub rec_atoms: Vec<RecordAtom>,
    rec_atom_map: HashMap<RecordAtom, u32>,
    pub tup_atoms: Vec<TupleAtom>,
    tup_atom_map: HashMap<TupleAtom, u32>,
    pub arrow_atoms: Vec<ArrowAtom>,
    arrow_atom_map: HashMap<ArrowAtom, u32>,

    pub rec_bdd: Bdd,
    pub tup_bdd: Bdd,
    pub arrow_bdd: Bdd,

    /// `mu` and nominal declarations. Never inlined, so an atom's identity does not
    /// depend on its declaration being resolved yet.
    pub defs: HashMap<NameId, TyId>,

    /// Reserved but not yet defined. Their `TyData` is `never` until `define`, so any
    /// eager read of it is a silent lie — hence `pending`.
    undefined: std::collections::HashSet<TyId>,
    pending: Vec<(TyId, PendingOp)>,

    pub nominal_label: NameId,
}

impl Default for Types {
    fn default() -> Self {
        Self::new()
    }
}

impl Types {
    pub fn new() -> Self {
        let mut t = Types {
            names: vec![],
            name_map: HashMap::new(),
            tys: vec![],
            ty_map: HashMap::new(),
            atomsets: vec![],
            atomset_map: HashMap::new(),
            rec_atoms: vec![],
            rec_atom_map: HashMap::new(),
            tup_atoms: vec![],
            tup_atom_map: HashMap::new(),
            arrow_atoms: vec![],
            arrow_atom_map: HashMap::new(),
            rec_bdd: Bdd::new(),
            tup_bdd: Bdd::new(),
            arrow_bdd: Bdd::new(),
            defs: HashMap::new(),
            undefined: std::collections::HashSet::new(),
            pending: Vec::new(),
            nominal_label: NameId(0),
        };
        // `#` is not an identifier character, so these cannot collide with source.
        t.nominal_label = t.name("#nominal");
        t
    }

    pub fn name(&mut self, s: &str) -> NameId {
        if let Some(&id) = self.name_map.get(s) {
            return id;
        }
        let id = NameId(self.names.len() as u32);
        self.names.push(s.to_string());
        self.name_map.insert(s.to_string(), id);
        id
    }

    pub fn name_str(&self, n: NameId) -> &str {
        &self.names[n.0 as usize]
    }

    pub fn arg_label(&mut self, i: usize) -> NameId {
        self.name(&format!("#{i}"))
    }

    // ---- interning ----

    fn atomset(&mut self, mut a: AtomSet) -> AtomSetId {
        a.names.sort_unstable();
        a.names.dedup();
        if let Some(&id) = self.atomset_map.get(&a) {
            return id;
        }
        let id = AtomSetId(self.atomsets.len() as u32);
        self.atomsets.push(a.clone());
        self.atomset_map.insert(a, id);
        id
    }

    pub fn atomset_of(&self, id: AtomSetId) -> &AtomSet {
        &self.atomsets[id.0 as usize]
    }

    pub fn intern(&mut self, d: TyData) -> TyId {
        if let Some(&id) = self.ty_map.get(&d) {
            return id;
        }
        let id = TyId(self.tys.len() as u32);
        self.tys.push(d);
        self.ty_map.insert(d, id);
        id
    }

    pub fn data(&self, t: TyId) -> TyData {
        self.tys[t.0 as usize]
    }

    /// Reserve an id before its body exists, so a `mu` type's body can refer to it.
    ///
    /// This is what makes recursion work without a `TypeRef` indirection: the id is
    /// stable, the body mentions it, and the graph simply has a cycle. Ids are
    /// finite, so nothing is infinite here — only `is_empty` has to notice the loop,
    /// which is what its assumption set is for.
    pub fn reserve(&mut self) -> TyId {
        let atoms = self.atomset(AtomSet::empty());
        let vars = self.atomset(AtomSet::empty());
        let id = TyId(self.tys.len() as u32);
        self.undefined.insert(id);
        self.tys.push(TyData {
            base: 0,
            atoms,
            vars,
            records: bdd::FALSE,
            tuples: bdd::FALSE,
            arrows: bdd::FALSE,
        });
        id
    }

    /// Fill in a reserved id. Registering it in `ty_map` afterwards is what makes the
    /// recursion equi-recursive: a later type with the same shape interns *to* this
    /// id, so `A` and its unfolding are one type with no fold/unfold and no wrapper.
    pub fn define(&mut self, id: TyId, d: TyData) {
        self.tys[id.0 as usize] = d;
        self.ty_map.entry(d).or_insert(id);
        self.undefined.remove(&id);
        self.discharge();
    }

    /// No reserved id is still awaiting its body.
    ///
    /// Querying while one is outstanding reads `never` for it and answers wrongly
    /// without saying so, which is the bug this whole mechanism exists to prevent.
    pub fn all_defined(&self) -> bool {
        self.undefined.is_empty()
    }

    fn ready(&self, op: PendingOp) -> bool {
        match op {
            PendingOp::Union(a, b) | PendingOp::Intersect(a, b) => {
                !self.undefined.contains(&a) && !self.undefined.contains(&b)
            }
            PendingOp::Negate(a) => !self.undefined.contains(&a),
        }
    }

    fn eval(&mut self, op: PendingOp) -> TyId {
        match op {
            PendingOp::Union(a, b) => self.union_eager(a, b),
            PendingOp::Intersect(a, b) => self.intersect_eager(a, b),
            PendingOp::Negate(a) => self.negate_eager(a),
        }
    }

    /// Re-run deferred ops now their operands exist, to a fixpoint.
    ///
    /// Contractivity is what bounds this: a recursive occurrence must sit beneath a
    /// constructor, so by the time the constructor is built the operand it defers on
    /// is defined. The recursion then closes through the constructor's raw `TyId`,
    /// which is the one path a boolean op never snapshots.
    fn discharge(&mut self) {
        loop {
            let ready: Vec<(TyId, PendingOp)> = self
                .pending
                .iter()
                .copied()
                .filter(|&(r, op)| self.undefined.contains(&r) && self.ready(op))
                .collect();
            if ready.is_empty() {
                break;
            }
            for (r, op) in ready {
                let id = self.eval(op);
                let d = self.data(id);
                self.tys[r.0 as usize] = d;
                self.ty_map.entry(d).or_insert(r);
                self.undefined.remove(&r);
            }
        }
        self.pending.retain(|&(r, _)| self.undefined.contains(&r));
    }

    /// Hand back a reserved id now and compute the op once its operands exist.
    fn defer(&mut self, op: PendingOp) -> TyId {
        let r = self.reserve();
        self.pending.push((r, op));
        r
    }

    pub fn rec_atom(&mut self, mut a: RecordAtom) -> u32 {
        a.fields.sort_by_key(|f| f.0);
        if let Some(&id) = self.rec_atom_map.get(&a) {
            return id;
        }
        let id = self.rec_atoms.len() as u32;
        self.rec_atoms.push(a.clone());
        self.rec_atom_map.insert(a, id);
        id
    }

    pub fn tup_atom(&mut self, a: TupleAtom) -> u32 {
        if let Some(&id) = self.tup_atom_map.get(&a) {
            return id;
        }
        let id = self.tup_atoms.len() as u32;
        self.tup_atoms.push(a.clone());
        self.tup_atom_map.insert(a, id);
        id
    }

    pub fn arrow_atom(&mut self, a: ArrowAtom) -> u32 {
        if let Some(&id) = self.arrow_atom_map.get(&a) {
            return id;
        }
        let id = self.arrow_atoms.len() as u32;
        self.arrow_atoms.push(a.clone());
        self.arrow_atom_map.insert(a, id);
        id
    }

    // ---- constructors ----

    pub fn never(&mut self) -> TyId {
        let atoms = self.atomset(AtomSet::empty());
        let vars = self.atomset(AtomSet::empty());
        self.intern(TyData {
            base: 0,
            atoms,
            vars,
            records: bdd::FALSE,
            tuples: bdd::FALSE,
            arrows: bdd::FALSE,
        })
    }

    /// `any` is ⊤ — every kind full. It is not an erasure marker; there is no such
    /// thing here to fall back to.
    pub fn any(&mut self) -> TyId {
        let atoms = self.atomset(AtomSet::all());
        let vars = self.atomset(AtomSet::all());
        self.intern(TyData {
            base: B_ANY,
            atoms,
            vars,
            records: bdd::TRUE,
            tuples: bdd::TRUE,
            arrows: bdd::TRUE,
        })
    }

    pub fn of_base(&mut self, b: BaseSet) -> TyId {
        let atoms = self.atomset(AtomSet::empty());
        let vars = self.atomset(AtomSet::empty());
        self.intern(TyData {
            base: b,
            atoms,
            vars,
            records: bdd::FALSE,
            tuples: bdd::FALSE,
            arrows: bdd::FALSE,
        })
    }

    pub fn i64(&mut self) -> TyId {
        self.of_base(B_I64)
    }
    pub fn f64(&mut self) -> TyId {
        self.of_base(B_F64)
    }
    pub fn str(&mut self) -> TyId {
        self.of_base(B_STR)
    }
    pub fn bool(&mut self) -> TyId {
        self.of_base(B_BOOL)
    }
    pub fn null(&mut self) -> TyId {
        self.of_base(B_NULL)
    }
    pub fn undef(&mut self) -> TyId {
        self.of_base(B_UNDEF)
    }

    /// Top of the *field* lattice: any value, or absent. Not the same as `any`, which
    /// is top of the value lattice and cannot be absent.
    pub fn any_or_undef(&mut self) -> TyId {
        let a = self.any();
        let u = self.undef();
        self.union(a, u)
    }

    pub fn atom(&mut self, n: NameId) -> TyId {
        let atoms = self.atomset(AtomSet { neg: false, names: vec![n] });
        let vars = self.atomset(AtomSet::empty());
        self.intern(TyData {
            base: 0,
            atoms,
            vars,
            records: bdd::FALSE,
            tuples: bdd::FALSE,
            arrows: bdd::FALSE,
        })
    }

    /// A rigid type variable: the `T` inside `fn show[T](x: T)`.
    ///
    /// Opaque and disjoint from every concrete type, which is what makes the body
    /// checkable once rather than per call site. The cost is that `x is i64` inside a
    /// generic body reads as a dead branch — sound for checking, since the body must
    /// hold for every `T`, but it means narrowing a type parameter is not a thing.
    pub fn var(&mut self, n: NameId) -> TyId {
        let atoms = self.atomset(AtomSet::empty());
        let vars = self.atomset(AtomSet { neg: false, names: vec![n] });
        self.intern(TyData {
            base: 0,
            atoms,
            vars,
            records: bdd::FALSE,
            tuples: bdd::FALSE,
            arrows: bdd::FALSE,
        })
    }

    pub fn record(&mut self, a: RecordAtom) -> TyId {
        let id = self.rec_atom(a);
        let b = self.rec_bdd.atom(id);
        let atoms = self.atomset(AtomSet::empty());
        let vars = self.atomset(AtomSet::empty());
        self.intern(TyData {
            base: 0,
            atoms,
            vars,
            records: b,
            tuples: bdd::FALSE,
            arrows: bdd::FALSE,
        })
    }

    pub fn tuple(&mut self, elems: Vec<TyId>) -> TyId {
        let id = self.tup_atom(TupleAtom { elems });
        let b = self.tup_bdd.atom(id);
        let atoms = self.atomset(AtomSet::empty());
        let vars = self.atomset(AtomSet::empty());
        self.intern(TyData {
            base: 0,
            atoms,
            vars,
            records: bdd::FALSE,
            tuples: b,
            arrows: bdd::FALSE,
        })
    }

    pub fn arrow(&mut self, params: Vec<TyId>, throws: TyId, ret: TyId) -> TyId {
        let id = self.arrow_atom(ArrowAtom { params, throws, ret });
        let b = self.arrow_bdd.atom(id);
        let atoms = self.atomset(AtomSet::empty());
        let vars = self.atomset(AtomSet::empty());
        self.intern(TyData {
            base: 0,
            atoms,
            vars,
            records: bdd::FALSE,
            tuples: bdd::FALSE,
            arrows: b,
        })
    }

    /// An open structural record: `{x: i64}`, which nominal records satisfy.
    /// `#nominal` is unconstrained, so a `Red` with an `x: i64` is a member.
    pub fn struct_ty(&mut self, fields: Vec<(NameId, TyId)>) -> TyId {
        let rest = self.any_or_undef();
        // Any nominal tag, or none: a `Red` with the right fields is a member, and so
        // is an anonymous record.
        let tag = {
            let a = self.atomset(AtomSet::all());
            let v = self.atomset(AtomSet::empty());
            self.intern(TyData {
                base: B_UNDEF,
                atoms: a,
                vars: v,
                records: bdd::FALSE,
                tuples: bdd::FALSE,
                arrows: bdd::FALSE,
            })
        };
        let nl = self.nominal_label;
        let mut fs = fields;
        fs.push((nl, tag));
        self.record(RecordAtom { fields: fs, rest })
    }

    /// A closed nominal record. `name` is its identity and `args` its generic
    /// arguments; both ride as reserved fields.
    pub fn nominal(&mut self, name: NameId, args: Vec<TyId>, fields: Vec<(NameId, TyId)>) -> TyId {
        let rest = self.undef();
        let tag = self.atom(name);
        let nl = self.nominal_label;
        let mut fs = fields;
        fs.push((nl, tag));
        for (i, a) in args.into_iter().enumerate() {
            let l = self.arg_label(i);
            fs.push((l, a));
        }
        self.record(RecordAtom { fields: fs, rest })
    }

    // ---- boolean operations, field-wise ----

    /// Union. Defers if either operand is a reserved id awaiting its body — reading
    /// its `TyData` now would snapshot `never` and drop the recursion on the floor.
    pub fn union(&mut self, a: TyId, b: TyId) -> TyId {
        if self.undefined.contains(&a) || self.undefined.contains(&b) {
            return self.defer(PendingOp::Union(a, b));
        }
        self.union_eager(a, b)
    }

    fn union_eager(&mut self, a: TyId, b: TyId) -> TyId {
        let (x, y) = (self.data(a), self.data(b));
        let atoms = {
            let (p, q) = (self.atomset_of(x.atoms).clone(), self.atomset_of(y.atoms).clone());
            self.atomset(atomset_or(&p, &q))
        };
        let vars = {
            let (p, q) = (self.atomset_of(x.vars).clone(), self.atomset_of(y.vars).clone());
            self.atomset(atomset_or(&p, &q))
        };
        let records = self.rec_bdd.or(x.records, y.records);
        let tuples = self.tup_bdd.or(x.tuples, y.tuples);
        let arrows = self.arrow_bdd.or(x.arrows, y.arrows);
        self.intern(TyData { base: x.base | y.base, atoms, vars, records, tuples, arrows })
    }

    pub fn intersect(&mut self, a: TyId, b: TyId) -> TyId {
        if self.undefined.contains(&a) || self.undefined.contains(&b) {
            return self.defer(PendingOp::Intersect(a, b));
        }
        self.intersect_eager(a, b)
    }

    fn intersect_eager(&mut self, a: TyId, b: TyId) -> TyId {
        let (x, y) = (self.data(a), self.data(b));
        let atoms = {
            let (p, q) = (self.atomset_of(x.atoms).clone(), self.atomset_of(y.atoms).clone());
            self.atomset(atomset_and(&p, &q))
        };
        let vars = {
            let (p, q) = (self.atomset_of(x.vars).clone(), self.atomset_of(y.vars).clone());
            self.atomset(atomset_and(&p, &q))
        };
        let records = self.rec_bdd.and(x.records, y.records);
        let tuples = self.tup_bdd.and(x.tuples, y.tuples);
        let arrows = self.arrow_bdd.and(x.arrows, y.arrows);
        self.intern(TyData { base: x.base & y.base, atoms, vars, records, tuples, arrows })
    }

    /// Complement within ⊤. `B_UNDEF` is included: a field's negation has to be able
    /// to say "absent", or `{x: i64} ∧ ¬{x: i64}` would not come out empty.
    pub fn negate(&mut self, a: TyId) -> TyId {
        if self.undefined.contains(&a) {
            return self.defer(PendingOp::Negate(a));
        }
        self.negate_eager(a)
    }

    fn negate_eager(&mut self, a: TyId) -> TyId {
        let x = self.data(a);
        let atoms = {
            let p = self.atomset_of(x.atoms).clone();
            self.atomset(AtomSet { neg: !p.neg, names: p.names })
        };
        let vars = {
            let p = self.atomset_of(x.vars).clone();
            self.atomset(AtomSet { neg: !p.neg, names: p.names })
        };
        let records = self.rec_bdd.not(x.records);
        let tuples = self.tup_bdd.not(x.tuples);
        let arrows = self.arrow_bdd.not(x.arrows);
        self.intern(TyData { base: !x.base & B_ALL, atoms, vars, records, tuples, arrows })
    }

    pub fn diff(&mut self, a: TyId, b: TyId) -> TyId {
        let nb = self.negate(b);
        self.intersect(a, nb)
    }

    pub fn union_all(&mut self, ts: &[TyId]) -> TyId {
        let mut acc = self.never();
        for &t in ts {
            acc = self.union(acc, t);
        }
        acc
    }

    pub fn intersect_all(&mut self, ts: &[TyId]) -> TyId {
        let mut acc = self.any();
        for &t in ts {
            acc = self.intersect(acc, t);
        }
        acc
    }
}

fn atomset_or(a: &AtomSet, b: &AtomSet) -> AtomSet {
    match (a.neg, b.neg) {
        // {a} ∪ {b}
        (false, false) => AtomSet {
            neg: false,
            names: a.names.iter().chain(&b.names).copied().collect(),
        },
        // ¬{a} ∪ ¬{b} = ¬({a} ∩ {b})
        (true, true) => AtomSet {
            neg: true,
            names: a.names.iter().filter(|n| b.names.contains(n)).copied().collect(),
        },
        // {a} ∪ ¬{b} = ¬({b} ∖ {a})
        (false, true) => AtomSet {
            neg: true,
            names: b.names.iter().filter(|n| !a.names.contains(n)).copied().collect(),
        },
        (true, false) => atomset_or(b, a),
    }
}

fn atomset_and(a: &AtomSet, b: &AtomSet) -> AtomSet {
    match (a.neg, b.neg) {
        (false, false) => AtomSet {
            neg: false,
            names: a.names.iter().filter(|n| b.names.contains(n)).copied().collect(),
        },
        // ¬{a} ∩ ¬{b} = ¬({a} ∪ {b})
        (true, true) => AtomSet {
            neg: true,
            names: a.names.iter().chain(&b.names).copied().collect(),
        },
        // {a} ∩ ¬{b} = {a} ∖ {b}
        (false, true) => AtomSet {
            neg: false,
            names: a.names.iter().filter(|n| !b.names.contains(n)).copied().collect(),
        },
        (true, false) => atomset_and(b, a),
    }
}

impl AtomSet {
    pub fn is_empty_set(&self) -> bool {
        self.is_empty()
    }
    pub fn has(&self, n: NameId) -> bool {
        self.contains(n)
    }
}
