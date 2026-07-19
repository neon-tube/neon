//! The type arena: what a type *is*, and how one is built. See
//! `docs/design/typechecker.md`.
//!
//! A type is a set of values, and `TyData` is that set written as six independent
//! components — the base primitives as a bitset, atoms and rigid variables as
//! finite-or-cofinite name sets, and records, tuples and arrows as BDDs over their own
//! kind's shapes. Union, intersection and complement are then component-wise, and there
//! is nothing to normalise afterwards: the representation is already closed under them.
//! That is the whole reason for the shape. A syntax tree of type constructors would need
//! a normaliser, and a normaliser is the thing that quietly disagrees with the subtype
//! checker.
//!
//! The split into components is not cosmetic. An `i64` is never a record and a record is
//! never an arrow, so no component's emptiness can depend on another's. `empty.rs` can
//! therefore decide each in isolation and memoize per kind, and `Types` can hold one
//! independent BDD arena per kind rather than one giant one.
//!
//! Everything is hash-consed: names, atom sets, type descriptors, and the three kinds of
//! shape atom. Equal descriptors get equal ids, so structural equality is a pointer
//! comparison. The converse does *not* hold — two ids can denote the same set of values
//! without being equal, so semantic questions go to `Solver::is_equiv`, never to `==`.
//!
//! **Nominal types have no machinery of their own.** A nominal record is an ordinary
//! record carrying a reserved `#nominal` field whose type is the atom of its name, and
//! generic arguments ride along the same way as `#0`, `#1`, .... `#` is not an identifier
//! character, so those labels cannot collide with anything from source. The payoff is
//! that nominal disjointness, nominal-satisfies-structural and generic covariance are not
//! three rules that have to be kept consistent — they are all the field-wise record
//! decomposition, which was going to be written anyway.
//!
//! Recursion works by reserving an id before its body exists (`reserve`/`define`), so a
//! `mu` type's body can name it and the graph simply has a cycle. The hazard that creates
//! is that a reserved id reads as `never` until it is filled in, and a boolean op that
//! snapshots one would silently drop the recursion — which is what `PendingOp` and
//! `discharge` exist to prevent, and what `all_defined` lets `empty.rs` assert against.

use super::bdd::{self, Bdd, BddId};
use std::collections::HashMap;

/// An interned string: a record label, an atom name, a nominal name, a type variable.
/// One namespace for all of them, which is what lets `#nominal` be an ordinary label
/// holding an ordinary atom.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct NameId(pub u32);

/// A type. Hash-consed, so equal ids denote equal sets of values — but not conversely;
/// use `Solver::is_equiv` for the real question.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct TyId(pub u32);

/// Which of the three shape arenas an operation is working in. Only `substitute` needs
/// it: the three kinds are otherwise addressed by separate fields and separate methods,
/// and this exists so the substitution walk can be written once instead of three times.
#[derive(Clone, Copy)]
enum Kind {
    Record,
    Tuple,
    Arrow,
}

// ---- base primitives ----

/// The primitive component of a type, as a bitset. Primitives are finite and mutually
/// disjoint, so union, intersection and complement are `|`, `&` and `!` — no BDD needed,
/// and the common case (`i64`, `str | null`) costs a machine word.
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

/// The primitives a value can actually have — what `any` admits.
pub const B_ANY: BaseSet = B_I64 | B_F64 | B_STR | B_BOOL | B_NULL;
/// The full lattice including absence, which is what complement is taken within. `B_ANY`
/// would be wrong there: negating a field type has to be able to say "absent", or
/// `{x: i64} ∧ ¬{x: i64}` would not come out empty.
pub const B_ALL: BaseSet = B_ANY | B_UNDEF;

// ---- atoms (`:ok`) ----

/// Atom names are countably infinite but any one type mentions finitely many, so
/// a set of them is finite or cofinite.
///
/// `neg` flips the sense of `names`, so a cofinite set is represented exactly rather than
/// approximated — `¬:ok` is `neg` with one name, not an enumeration of everything else.
/// `names` is kept sorted and deduplicated by `Types::atomset`; both the hash-consing and
/// the binary search in `contains` depend on that, so an `AtomSet` built by hand and not
/// passed through `atomset` is outside the invariant.
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
    /// Only a positive set with no names is empty. A negated set never is, however many
    /// names it excludes, because the universe of atom names is infinite — which is also
    /// why `empty.rs` can treat a non-empty atom component as proof of inhabitation
    /// without looking at what the names are.
    fn is_empty(&self) -> bool {
        !self.neg && self.names.is_empty()
    }
    fn contains(&self, n: NameId) -> bool {
        self.names.binary_search(&n).is_ok() != self.neg
    }
}

/// An interned `AtomSet`. `TyData` holds these rather than the sets themselves so that it
/// stays `Copy` and cheap to hash — it is compared and cloned on every boolean operation.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct AtomSetId(pub u32);

/// The state one `substitute` call carries down its walk.
///
/// `active` is the types currently being rewritten, i.e. the cycle guard. The `Option`
/// is lazy on purpose: a type only gets a reserved id if something actually re-enters it,
/// so an acyclic type is rebuilt through the ordinary constructors and keeps whatever id
/// hash-consing gives it. Reserving up front would instead run `define` on every
/// intermediate result, which overwrites `ty_map` and would move canonicity around for
/// types that were never recursive to begin with.
///
/// `done` memoizes finished rewrites, so a type shared by several fields is rebuilt once
/// rather than once per path to it.
#[derive(Default)]
struct Progress {
    active: HashMap<TyId, Option<TyId>>,
    done: HashMap<TyId, TyId>,
}

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
    /// The type of `label` in this shape — `rest` when the shape does not name it.
    ///
    /// The totality is the point: `empty.rs` meets two record atoms label by label over
    /// the union of their labels, and this lets it do so without first widening either
    /// atom to a common label set. Falling back to `rest` is also what distinguishes an
    /// open shape (`rest` admits anything) from a closed one (`rest` is `undef`), with no
    /// open/closed flag anywhere.
    ///
    /// The binary search assumes `fields` is sorted, which `Types::rec_atom` ensures.
    pub fn get(&self, label: NameId) -> TyId {
        match self.fields.binary_search_by_key(&label, |f| f.0) {
            Ok(i) => self.fields[i].1,
            Err(_) => self.rest,
        }
    }
    /// The labels this shape names explicitly, `#nominal` and `#0`… included. Not a list
    /// of the labels a value of it may have — everything else is governed by `rest`.
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

/// The arena every type in a compilation lives in.
///
/// Each `Vec`/`HashMap` pair is one hash-consing table: the vector maps id to value, the
/// map maps value back to id. Ids are indices, never reused and never invalidated, so
/// anything downstream may hold a `TyId` indefinitely — but only against *this* arena.
/// There is one `Types` per compilation for exactly that reason.
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

    /// The interned `#nominal`, cached because it is needed on every record construction.
    pub nominal_label: NameId,

    /// Nominal record name to the C type the runtime owns it as, from `@runtime("...")`.
    ///
    /// It lives here because `repr_of` — which decides a type's representation — takes
    /// only a `&Types`, and a type's identity by that point is just its nominal name.
    /// Putting the map anywhere else would mean threading a second table through every
    /// caller of the representation map.
    pub runtime_types: std::collections::HashMap<String, String>,
}

impl Default for Types {
    fn default() -> Self {
        Self::new()
    }
}

impl Types {
    /// A fresh arena. `#nominal` is interned here rather than lazily so that
    /// `nominal_label` is valid from the first use — every record construction reads it,
    /// and a lazily interned label would mean `record` needed `&mut self` reasoning it
    /// otherwise does not.
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
            runtime_types: std::collections::HashMap::new(),
        };
        // `#` is not an identifier character, so these cannot collide with source.
        t.nominal_label = t.name("#nominal");
        t
    }

    /// Intern a string. Every label, atom, nominal name and type variable shares this one
    /// table, so equal spellings are one `NameId` and name comparison is an integer
    /// compare — which record field lookup and atom-set membership both rely on.
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

    /// The reserved label a nominal type's i-th generic argument rides in.
    ///
    /// `List[i64]` is a record with a `#0` field of type `i64`, so `List[i64] <: List[any]`
    /// is decided by the same field-wise rule as any other record — generic variance is
    /// not a separate rule, and cannot disagree with one.
    pub fn arg_label(&mut self, i: usize) -> NameId {
        self.name(&format!("#{i}"))
    }

    // ---- interning ----

    /// Intern an atom set, canonicalising it first. The sort and dedup are load-bearing
    /// twice over: without them two spellings of the same set would intern to different
    /// ids and defeat the hash-consing, and `AtomSet::contains` would binary-search an
    /// unsorted vector and answer wrongly. Every `AtomSet` reaching `TyData` goes through
    /// here, which is what makes that invariant hold globally.
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

    /// Hash-cons a descriptor. Identical descriptors collapse to one id, which is what
    /// makes `TyId` equality a sound (if incomplete) type equality and keeps the memo
    /// tables in `empty.rs` small — the same type asked about twice is the same key.
    ///
    /// Incomplete, because two descriptors can denote the same set without being equal
    /// as descriptors. The components are canonical individually, not jointly.
    pub fn intern(&mut self, d: TyData) -> TyId {
        if let Some(&id) = self.ty_map.get(&d) {
            return id;
        }
        let id = TyId(self.tys.len() as u32);
        self.tys.push(d);
        self.ty_map.insert(d, id);
        id
    }

    /// The descriptor behind an id. Beware of calling this on an id that `reserve`
    /// handed out and `define` has not yet filled: the answer is a well-formed `never`
    /// rather than an error, and that silence is the failure mode `all_defined` and the
    /// deferral machinery below exist to close off.
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
    ///
    /// The insert OVERWRITES on purpose. A body is interned while it is being built,
    /// before `define` runs, so an id for this shape usually already exists — and
    /// `or_insert` would leave that one canonical and the reserved one a synonym.
    /// Two ids for one type defeats hash-consing, which is the thing that makes `==`
    /// a legitimate way to compare types: `Circle` would be one id by name and
    /// another through any boolean op, and an id comparison would silently answer no.
    pub fn define(&mut self, id: TyId, d: TyData) {
        self.tys[id.0 as usize] = d;
        self.ty_map.insert(d, id);
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

    /// Whether a deferred op's operands have bodies yet. Only definedness is checked —
    /// an operand may itself be a recursive type, which is fine; what must not happen is
    /// reading a slot that is still a placeholder.
    fn ready(&self, op: PendingOp) -> bool {
        match op {
            PendingOp::Union(a, b) | PendingOp::Intersect(a, b) => {
                !self.undefined.contains(&a) && !self.undefined.contains(&b)
            }
            PendingOp::Negate(a) => !self.undefined.contains(&a),
        }
    }

    /// Run a deferred op for real. It dispatches to the `_eager` variants deliberately:
    /// the public entry points would re-check definedness and could defer again, and
    /// `ready` has already established there is nothing left to wait for.
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
    ///
    /// The outer loop is a fixpoint because discharging one op defines its result, which
    /// can be the operand another op was waiting on. The final `retain` drops everything
    /// now defined and leaves the still-blocked ops for the next `define`.
    ///
    /// Note the `or_insert` here, where `define` deliberately overwrites. It reads like a
    /// choice between two mappings, but it is not one: every arm of `eval` ends in
    /// `intern`, so `d` is already a key of `ty_map` by the time this line runs and the
    /// insert can never fire. Verified by probing it under the suite — zero hits.
    ///
    /// What it therefore states is the outcome, not a policy: the reserved id becomes a
    /// second id carrying the same descriptor rather than displacing the canonical one.
    /// That duplicate is unavoidable — `r` may already be embedded in interned shapes, so
    /// it cannot be rewritten away, and `insert` instead of `or_insert` would merely move
    /// which of the two is canonical. Callers holding the reserved id have a type that is
    /// equivalent to, but not `==`, the canonical one, which is one of the reasons
    /// `is_equiv` and not `==` is the supported way to compare types.
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
    ///
    /// The caller cannot tell the difference, which is the point — `union` inside a
    /// recursive declaration returns something usable immediately instead of forcing
    /// every construction site to know whether it is inside a cycle.
    fn defer(&mut self, op: PendingOp) -> TyId {
        let r = self.reserve();
        self.pending.push((r, op));
        r
    }

    /// Intern a record shape and return its index in the record BDD's variable order.
    ///
    /// Sorting the fields is what `RecordAtom::get`'s binary search assumes, and it also
    /// makes field order irrelevant to identity: `{x, y}` and `{y, x}` are one atom, so
    /// they are one BDD variable and compare equal without any set-comparison work.
    ///
    /// The sort is *not* a dedup, and must not be: with a label twice, `get`'s binary
    /// search would return whichever of the two it happened to land on, so `{x: i64,
    /// x: str}` would silently be one of them rather than an error. Uniqueness is the
    /// caller's invariant, and `env.rs::record_body` enforces it by rejecting a duplicate
    /// field on the declaration before a shape is ever built. Deduping here would turn
    /// that diagnostic into a coin flip.
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

    /// Intern a tuple shape. Unlike records, nothing is canonicalised: element order is
    /// meaningful, so the value as given is already the identity.
    pub fn tup_atom(&mut self, a: TupleAtom) -> u32 {
        if let Some(&id) = self.tup_atom_map.get(&a) {
            return id;
        }
        let id = self.tup_atoms.len() as u32;
        self.tup_atoms.push(a.clone());
        self.tup_atom_map.insert(a, id);
        id
    }

    /// Intern a function shape. `throws` is part of the identity, so two functions
    /// differing only in what they may throw are different atoms and neither is a
    /// subtype of the other by construction — the relation between them is decided by
    /// `arrow_le`, which compares the throws component alongside the return.
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

    /// `never` — ⊥, every component empty. The identity for `union` and the type of an
    /// expression that does not return; also what an absent `throws` clause must resolve
    /// to, since `any` there would make every call site think it can fail.
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

    /// `any` is ⊤ of the *value* lattice: every kind full, every atom and every variable
    /// admitted — but `B_ANY` rather than `B_ALL`, so it excludes `undef`. A value is
    /// never absent, so `any` is not the top of the field lattice; `any_or_undef` is.
    ///
    /// It is a real type here, not an erasure marker. There is no unknown case in this
    /// representation for it to stand in for.
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

    /// A type that is nothing but the given primitives. Passing several bits at once is
    /// exactly the union of them, so `of_base(B_I64 | B_NULL)` is `i64 | null` with no
    /// boolean operation performed.
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
    /// The type of an absent field. Unwritable in source, and used as a record atom's
    /// `rest` to close it: a shape whose unnamed labels must all be absent is a shape
    /// that admits no extra fields.
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

    /// A singleton atom type such as `:ok`. Distinct names are disjoint automatically —
    /// two singletons meet to the empty name set — which is what makes an enum's variants
    /// mutually exclusive without an enum rule, and what carries nominal identity in the
    /// `#nominal` field.
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

    /// A type consisting of exactly one record shape. It goes into the record BDD as a
    /// single variable; unions, intersections and negations of records are then the BDD's
    /// job, not this constructor's, and no combination of shapes ever needs a new atom.
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

    /// A single tuple shape. Also used for something that is not a source-level tuple:
    /// `arrow_le` compares whole parameter lists by wrapping them as tuples, so
    /// multi-parameter domain subtyping reuses the tuple decomposition.
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

    /// A single function shape. `throws` is `never` for a function that cannot fail; see
    /// `ArrowAtom::throws` for why that must not default to `any`.
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

    /// The signature of `ty`, when it is exactly one arrow — `(A) -> B`, not a union
    /// or intersection of them. First-class calls need the params and return of the
    /// value being called, and a lone arrow is the case that has an unambiguous one;
    /// an overloaded `(A->B) & (C->D)` is not applied in v1.
    pub fn as_arrow(&self, ty: TyId) -> Option<ArrowAtom> {
        let d = self.data(ty);
        if d.base != 0 || d.records != bdd::FALSE || d.tuples != bdd::FALSE {
            return None;
        }
        if !self.atomset_of(d.atoms).is_empty_set() || !self.atomset_of(d.vars).is_empty_set() {
            return None;
        }
        match self.arrow_bdd.paths(d.arrows).as_slice() {
            [(pos, neg)] if neg.is_empty() && pos.len() == 1 => {
                Some(self.arrow_atoms[pos[0] as usize].clone())
            }
            _ => None,
        }
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
    ///
    /// Closed means `rest` is `undef`: every label the declaration does not name must be
    /// absent, so a record with an extra field is not a member. Because the tag is a
    /// singleton atom, two different nominals meet to `never` in `#nominal` and are
    /// disjoint with no rule stated; because the rest of the fields are ordinary, such a
    /// value still satisfies any structural shape it happens to fit.
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

    /// Union with both bodies known: component-wise, with no cross-component work at all.
    /// The same shape repeats in `intersect_eager` and `negate_eager`, and that repetition
    /// is the representation paying off — there is no case analysis over type
    /// constructors anywhere in this file.
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

    /// Intersection, deferring on an undefined operand for the same reason `union` does.
    ///
    /// Note that this builds the intersection, it does not decide anything about it: the
    /// result may well be uninhabited, and only `Solver::is_empty` can say. That
    /// separation is deliberate — construction stays cheap and total, and the expensive
    /// question is asked once, where it is actually needed.
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

    /// Complement with the body known. Each component negates in place — the base bitset
    /// within `B_ALL`, the two name sets by flipping `neg`, the three BDDs by `not` — so
    /// negation costs no more than the other two operations and needs no normalisation
    /// afterwards. Closure under complement is what allows subtyping to be phrased as
    /// emptiness of `a ∧ ¬b` at all.
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

    /// `a ∖ b`. The operation narrowing is written in — `x is T` leaves `t ∧ T` on one
    /// branch and `t ∖ T` on the other — and the one subtyping reduces to.
    pub fn diff(&mut self, a: TyId, b: TyId) -> TyId {
        let nb = self.negate(b);
        self.intersect(a, nb)
    }

    /// Replace rigid variables by their bindings, everywhere they occur.
    ///
    /// A type is not a tree, so this cannot be a tree rewrite. It instead takes the
    /// descriptor apart and rebuilds it: the primitives and atoms carry over unchanged,
    /// each variable is replaced by its binding (or by itself, when unbound), and each
    /// kind's BDD is reconstructed from its DNF paths with every shape's component types
    /// substituted. `List[T]` with `T := i64` becomes `List[i64]`; a bare `T` becomes its
    /// binding.
    ///
    /// The variable component is finite or cofinite and the two cases are *not* the same
    /// walk, which is the trap this used to fall into. For a finite set, `vars.names`
    /// lists the members, and each is replaced by its binding — the substituted name must
    /// disappear from the result. For a cofinite set the names are the ones *excluded*,
    /// so walking them substitutes the wrong things and, worse, drops every member. That
    /// is exactly the variable component of `any`, which is ⊤: `substitute(any, {T:=i64})`
    /// came back as "any value that is not a rigid variable", and passing a `U` to an
    /// `any` parameter of a *generic* function was then rejected with
    /// `expected !<var>, found U` — while the identical non-generic signature accepted it,
    /// because `substitute` returns early on an empty substitution. A cofinite component
    /// is therefore carried through unchanged (⊤ mentions no variable, so σ(⊤) = ⊤) with
    /// the images of any bound members unioned on.
    ///
    /// A recursive type is a *cycle* in the graph, not a tree, so the walk has to be
    /// guarded or it does not terminate. It used to not be, and the doc comment here
    /// said so and asked callers not to hand one over — which nothing enforced and
    /// ordinary source violated: `record Node { next: Node | null }` passed to any
    /// generic function (`fn f[T](n: Node, x: T) -> T`) overflowed the compiler's stack,
    /// as did the generic-and-recursive `record Tree[T] { kid: Tree[T] | null, v: T }`.
    /// `Progress` below closes that: re-entering a type in progress hands back a reserved
    /// id, which `define` fills in once the body is rebuilt — the same reserve/define
    /// cycle `env.rs` uses to build the recursive type in the first place. Completed
    /// types are memoized, so a shared (but acyclic) subtype is rebuilt once.
    pub fn substitute(&mut self, ty: TyId, subst: &HashMap<NameId, TyId>) -> TyId {
        if subst.is_empty() {
            return ty;
        }
        let mut p = Progress::default();
        self.subst_rec(ty, subst, &mut p)
    }

    fn subst_rec(&mut self, ty: TyId, subst: &HashMap<NameId, TyId>, p: &mut Progress) -> TyId {
        if let Some(&done) = p.done.get(&ty) {
            return done;
        }
        match p.active.get(&ty) {
            // Re-entry through a constructor: hand back a placeholder for this very type
            // and let the constructor close the cycle around it.
            Some(&Some(r)) => return r,
            Some(&None) => {
                let r = self.reserve();
                p.active.insert(ty, Some(r));
                return r;
            }
            None => {}
        }
        p.active.insert(ty, None);
        let out = self.subst_body(ty, subst, p);
        let slot = p.active.remove(&ty).flatten();
        let out = match slot {
            Some(r) => {
                // Contractivity puts every recursive occurrence under a constructor, and
                // constructors are always defined, so `out` cannot still be a deferred
                // boolean op here. If it ever were, `data` would read the `never`
                // placeholder and this would define the recursion away in silence.
                assert!(
                    !self.undefined.contains(&out),
                    "substitute: a cyclic result was still a deferred boolean op"
                );
                let d = self.data(out);
                self.define(r, d);
                r
            }
            None => out,
        };
        p.done.insert(ty, out);
        out
    }

    fn subst_body(&mut self, ty: TyId, subst: &HashMap<NameId, TyId>, p: &mut Progress) -> TyId {
        let d = self.data(ty);
        let mut acc = self.never();

        if d.base != 0 {
            let b = self.of_base(d.base);
            acc = self.union(acc, b);
        }
        if !self.atomset_of(d.atoms).is_empty_set() {
            let mut e = self.empty_data();
            e.atoms = d.atoms;
            let t = self.intern(e);
            acc = self.union(acc, t);
        }

        let vars = self.atomset_of(d.vars).clone();
        if vars.neg {
            // Cofinite: `names` are the *excluded* variables, so the members cannot be
            // enumerated. Carry the component through as it stands and add the image of
            // every bound variable that is a member — for `any` (`neg` with no names)
            // that reproduces `any` exactly, which is the case that matters.
            let mut e = self.empty_data();
            e.vars = d.vars;
            let t = self.intern(e);
            acc = self.union(acc, t);
            // Sorted because `subst` is a `HashMap`: the union is commutative so the
            // result is the same either way, but the *intermediate* types it interns are
            // not, and iteration order would otherwise leak into the arena's id numbering
            // and make a build unreproducible.
            let mut bound: Vec<TyId> =
                subst.iter().filter(|(n, _)| vars.has(**n)).map(|(_, &t)| t).collect();
            bound.sort_unstable();
            for t in bound {
                acc = self.union(acc, t);
            }
        } else {
            for name in &vars.names {
                let t = subst.get(name).copied().unwrap_or_else(|| self.var(*name));
                acc = self.union(acc, t);
            }
        }

        acc = self.subst_kind(acc, d.records, subst, Kind::Record, p);
        acc = self.subst_kind(acc, d.tuples, subst, Kind::Tuple, p);
        acc = self.subst_kind(acc, d.arrows, subst, Kind::Arrow, p);
        acc
    }

    /// A blank `never` descriptor to fill in. It takes `&mut self` because even the empty
    /// atom set has to be interned before it can be named by an id.
    fn empty_data(&mut self) -> TyData {
        let atoms = self.atomset(AtomSet::empty());
        let vars = self.atomset(AtomSet::empty());
        TyData { base: 0, atoms, vars, records: bdd::FALSE, tuples: bdd::FALSE, arrows: bdd::FALSE }
    }

    /// ⊤ restricted to one kind: every record, or every tuple, or every function, and
    /// nothing else. It is the seed a DNF path is rebuilt from in `subst_kind`, since a
    /// path with no positive atoms denotes the whole kind rather than nothing.
    fn kind_top(&mut self, kind: Kind) -> TyId {
        let mut d = self.empty_data();
        match kind {
            Kind::Record => d.records = bdd::TRUE,
            Kind::Tuple => d.tuples = bdd::TRUE,
            Kind::Arrow => d.arrows = bdd::TRUE,
        }
        self.intern(d)
    }

    /// Substitute inside one kind's BDD, by going out to DNF and coming back.
    ///
    /// There is no way to rewrite a BDD in place here: substitution changes the *atoms*,
    /// and a rewritten atom is a different variable in a different position of the order,
    /// so the diagram has to be rebuilt from `union`/`intersect`/`diff` rather than
    /// edited. The result is unioned onto `acc`, which is why this takes and returns the
    /// accumulator instead of composing at the call site.
    fn subst_kind(
        &mut self,
        acc: TyId,
        bdd: BddId,
        subst: &HashMap<NameId, TyId>,
        kind: Kind,
        p: &mut Progress,
    ) -> TyId {
        let paths = match kind {
            Kind::Record => self.rec_bdd.paths(bdd),
            Kind::Tuple => self.tup_bdd.paths(bdd),
            Kind::Arrow => self.arrow_bdd.paths(bdd),
        };
        let mut acc = acc;
        for (pos, neg) in paths {
            let mut pt = self.kind_top(kind);
            for i in pos {
                let a = self.subst_atom(i, subst, kind, p);
                pt = self.intersect(pt, a);
            }
            for j in neg {
                let a = self.subst_atom(j, subst, kind, p);
                pt = self.diff(pt, a);
            }
            acc = self.union(acc, pt);
        }
        acc
    }

    /// Substitute inside one shape, returning a type holding the rewritten shape alone.
    ///
    /// `rest` and `throws` are substituted along with the visible components. Missing
    /// either would be silent: a generic open record would lose its openness, and a
    /// generic throwing function would come back as one that cannot fail.
    fn subst_atom(
        &mut self,
        idx: u32,
        subst: &HashMap<NameId, TyId>,
        kind: Kind,
        p: &mut Progress,
    ) -> TyId {
        match kind {
            Kind::Record => {
                let a = self.rec_atoms[idx as usize].clone();
                let fields = a
                    .fields
                    .iter()
                    .map(|&(l, t)| (l, self.subst_rec(t, subst, p)))
                    .collect();
                let rest = self.subst_rec(a.rest, subst, p);
                self.record(RecordAtom { fields, rest })
            }
            Kind::Tuple => {
                let a = self.tup_atoms[idx as usize].clone();
                let elems = a.elems.iter().map(|&t| self.subst_rec(t, subst, p)).collect();
                self.tuple(elems)
            }
            Kind::Arrow => {
                let a = self.arrow_atoms[idx as usize].clone();
                let params =
                    a.params.iter().map(|&t| self.subst_rec(t, subst, p)).collect();
                let throws = self.subst_rec(a.throws, subst, p);
                let ret = self.subst_rec(a.ret, subst, p);
                self.arrow(params, throws, ret)
            }
        }
    }

    /// Fold a union, starting from `never` so that the empty slice gives `never` — the
    /// identity, and the right answer for an enum with no variants or a match with no
    /// arms.
    pub fn union_all(&mut self, ts: &[TyId]) -> TyId {
        let mut acc = self.never();
        for &t in ts {
            acc = self.union(acc, t);
        }
        acc
    }

    /// Fold an intersection, starting from `any` — the identity, so an empty slice
    /// constrains nothing. Note that `any` excludes `undef`, so this is not usable for
    /// folding *field* types, where the identity is `any_or_undef`.
    pub fn intersect_all(&mut self, ts: &[TyId]) -> TyId {
        let mut acc = self.any();
        for &t in ts {
            acc = self.intersect(acc, t);
        }
        acc
    }
}

/// Union of two finite-or-cofinite name sets, by cases on their signs.
///
/// The point of the case split is that the result stays in the same representation: a
/// union involving a cofinite set is cofinite, and the identities in the comments turn it
/// back into a `neg` set over a *finite* list of names. Nothing here ever enumerates the
/// universe, which is what makes negation of an atom type cheap and exact.
///
/// The result is not sorted; `Types::atomset` canonicalises it.
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

/// Intersection of two finite-or-cofinite name sets, the dual of `atomset_or`. The
/// `(true, false)` arm recurses with the operands swapped rather than repeating the
/// `(false, true)` algebra, which is safe because intersection is commutative and the
/// swapped call cannot hit the same arm again.
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

/// Public wrappers over the private predicates above. They exist so that other modules
/// can ask about an atom set without the private methods becoming part of the API by
/// accident, and `is_empty_set` is spelled unlike `is_empty` on purpose: a *negated*
/// atom set with no names is `all`, not empty, and a reader reaching for the familiar
/// `is_empty` should be made to look.
impl AtomSet {
    pub fn is_empty_set(&self) -> bool {
        self.is_empty()
    }
    pub fn has(&self, n: NameId) -> bool {
        self.contains(n)
    }
}

#[cfg(test)]
mod tests {
    use super::super::empty::Solver;
    use super::*;

    /// `substitute` used to walk `vars.names` unconditionally. For `any` that set is
    /// cofinite — `neg` with no names — so the walk visited nothing and the whole
    /// variable component was dropped, leaving "any value that is not a rigid variable".
    /// Since `substitute` returns early on an empty substitution, the loss showed up only
    /// for *generic* callees: `fn pick[T](a: T, b: any)` rejected a rigid `U` for `b`
    /// with `expected !<var>, found U`, while the identical non-generic signature took it.
    #[test]
    fn substitute_preserves_the_top_variable_component() {
        let mut s = Solver::new();
        let t = s.t.name("T");
        let i = s.t.i64();
        let any = s.t.any();
        let sub = HashMap::from([(t, i)]);
        let out = s.t.substitute(any, &sub);
        assert_eq!(out, any, "sigma(any) is any");

        let u = s.t.name("U");
        let uv = s.t.var(u);
        assert!(s.is_subtype(uv, out), "a rigid variable is still a member of any");
    }

    /// The same loss, one level down: an open structural record's `rest` is
    /// `any_or_undef`, whose variable component is also cofinite.
    #[test]
    fn substitute_preserves_the_top_of_the_field_lattice() {
        let mut s = Solver::new();
        let t = s.t.name("T");
        let i = s.t.i64();
        let top = s.t.any_or_undef();
        let sub = HashMap::from([(t, i)]);
        let out = s.t.substitute(top, &sub);
        assert!(s.is_equiv(out, top), "sigma(any|undef) is any|undef");
    }

    /// Build `Node = {next: Node | null}` the way `env.rs` does, then substitute into it.
    /// Before the cycle guard this walked the graph as if it were a tree and overflowed
    /// the compiler's stack — reachable from ordinary source, e.g.
    /// `record Node { next: Node | null }` passed to any generic function.
    #[test]
    fn substitute_terminates_on_a_recursive_record() {
        let mut s = Solver::new();
        let node = s.t.reserve();
        let null = s.t.null();
        let next = s.t.union(node, null);
        let (nm, lbl) = (s.t.name("Node"), s.t.name("next"));
        let body = s.t.nominal(nm, vec![], vec![(lbl, next)]);
        let d = s.t.data(body);
        s.t.define(node, d);
        assert!(s.t.all_defined());

        let t = s.t.name("T");
        let i = s.t.i64();
        let sub = HashMap::from([(t, i)]);
        let out = s.t.substitute(node, &sub);
        assert!(s.t.all_defined(), "the cycle guard's reserved id was filled in");
        assert!(s.is_equiv(out, node), "Node mentions no T, so sigma(Node) is Node");
    }

    /// The recursive *and* generic case: the walk has real work to do at every level, so
    /// the guard cannot be a "no variables occur, return unchanged" shortcut.
    #[test]
    fn substitute_rewrites_a_recursive_generic_record() {
        let mut s = Solver::new();
        let t = s.t.name("T");
        let tv = s.t.var(t);
        let tree = s.t.reserve();
        let null = s.t.null();
        let kid = s.t.union(tree, null);
        let (nm, kl, vl) = (s.t.name("Tree"), s.t.name("kid"), s.t.name("v"));
        let body = s.t.nominal(nm, vec![tv], vec![(kl, kid), (vl, tv)]);
        let d = s.t.data(body);
        s.t.define(tree, d);
        assert!(s.t.all_defined());

        let i = s.t.i64();
        let sub = HashMap::from([(t, i)]);
        let out = s.t.substitute(tree, &sub);
        assert!(s.t.all_defined());
        assert!(!s.is_equiv(out, tree), "T was rewritten, so this is a different type");

        // The `#0` generic argument and the `v` field both came out as `i64`.
        let od = s.t.data(out);
        let paths = s.t.rec_bdd.paths(od.records);
        let [(pos, neg)] = paths.as_slice() else { panic!("expected one cube: {paths:?}") };
        assert!(neg.is_empty() && pos.len() == 1, "expected a single record shape");
        let atom = s.t.rec_atoms[pos[0] as usize].clone();
        let arg0 = s.t.arg_label(0);
        assert_eq!(atom.get(arg0), i, "Tree[T] with T:=i64 is Tree[i64]");
        assert_eq!(atom.get(vl), i, "the v field is i64");
        // And the recursion survived: `kid` still leads back to a record.
        let kd = s.t.data(atom.get(kl));
        assert_ne!(kd.records, bdd::FALSE, "kid is still Tree[i64] | null");
    }
}
