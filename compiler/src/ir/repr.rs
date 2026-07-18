//! The representation map: `TyId → Repr`, total by construction.
//!
//! This is the map whose *absence* was the graveyard's undoing — lowering could not
//! answer "what does this type look like in memory", fell back to `Erased`, and the
//! erasure metastasised. Here every type has a representation and there is no unknown
//! case: `repr_of` is total. The one abstract result, `Var`, exists only inside a
//! generic body and is gone after monomorphisation; the one erased result, `Any`, is
//! the single boundary the checker already fences off.
//!
//! `Repr` is fully structural — an aggregate carries its fields' reprs, a list its
//! element's — so the value-witness a generic container needs (`size`, `retain`,
//! `release`, `drop`) is a pure function of the `Repr`, computed later without looking
//! anything up.

use std::collections::{HashMap, HashSet};

use crate::typecheck::bdd;
use crate::typecheck::types::{
    NameId, Types, B_ANY, B_BOOL, B_F64, B_I64, B_NULL, B_STR, TyId,
};

/// How a value of some type is laid out. See the module docs and `docs/design/ir.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Repr {
    /// 64-bit integer.
    I64,
    /// 64-bit float.
    F64,
    /// One byte.
    Bool,
    /// The runtime `str`: a flat refcounted byte buffer (or an immortal literal).
    Str,
    /// The `null` value: a zero-payload tag.
    Null,
    /// `()` — the empty tuple, zero size.
    Unit,
    /// An atom or a union of atoms: a 64-bit hashed discriminant.
    Tag,
    /// A record or nominal struct, stored inline. `name` is the nominal identity (for
    /// the printer and for union discriminants); `None` for an anonymous record.
    Record { name: Option<String>, fields: Vec<(String, Repr)> },
    /// A tuple, stored inline in order.
    Tuple(Vec<Repr>),
    /// A `List[T]` — a runtime container holding `T` inline.
    List(Box<Repr>),
    /// A `Map[K, V]` — an immutable HAMT.
    Map(Box<Repr>, Box<Repr>),
    /// A closure: a function pointer plus a boxed environment.
    Closure { params: Vec<Repr>, ret: Box<Repr> },
    /// A tagged union of two or more distinct variants.
    Union(Vec<Repr>),
    /// `T | null` where `T` is pointer-backed: a nullable pointer, `null` = null pointer.
    Nullable(Box<Repr>),
    /// A rigid type variable, abstract until monomorphisation. A `repr_of` result of
    /// `Var` in a fully monomorphic program is a bug the no-gaps test catches.
    Var(String),
    /// A pointer to a heap-allocated recursive record, carried by its record *atom*. A
    /// record whose cycle closes entirely by value has no finite inline layout, so every
    /// value of it is this pointer — uniformly, at every use.
    BoxedRec(u32),
    /// A back-edge of a recursive (`mu`) type: a pointer to the enclosing type, carried
    /// by its `TyId`. This is what gives a recursive type a finite representation, and
    /// it is the runtime indirection the recursion sits behind.
    Recursive(TyId),
    /// The one erasure boundary: `any`, a boxed value with a header and a type tag.
    Any,
    /// Uninhabited — a `never`, the type of an expression that does not return.
    Never,
}

impl Repr {
    /// Whether a value of this repr lives behind a pointer, so `T | null` can use a
    /// null pointer rather than a discriminant.
    /// A back-edge is deliberately *not* a pointer. It names a type without describing
    /// it, and the type it names may well be an inline union — `mu type A = :ok | List[A]`
    /// is a `{tag, payload}` by value, whose recursion terminates through the list's
    /// pointer, not through the back-edge. Calling it a pointer had the refcount pass
    /// emit `neon_retain((neon_header*)x)` against a stack union.
    pub fn is_pointer(&self) -> bool {
        matches!(
            self,
            Repr::Str
                | Repr::List(_)
                | Repr::Map(_, _)
                | Repr::Closure { .. }
                | Repr::BoxedRec(_)
        )
    }

    /// Whether a value of this repr owns anything reference-counted — either it is a
    /// counted pointer itself, or an aggregate with one somewhere inside.
    ///
    /// This, not `is_pointer`, decides whether the refcount pass tracks a value. Gating on
    /// `is_pointer` left every aggregate untracked, so a union, record or tuple holding a
    /// string or a list was never released: its parts were counted when a witness walked
    /// them, but a value sitting in a local simply leaked.
    pub fn is_counted(&self) -> bool {
        match self {
            Repr::Str
            | Repr::List(_)
            | Repr::Map(_, _)
            | Repr::Closure { .. }
            | Repr::BoxedRec(_)
            | Repr::Any => true,
            // A back-edge resolves to its unfolding in the emitter; assume it counts and
            // let `rc_parts` emit nothing when the resolved shape holds nothing counted.
            Repr::Recursive(_) => true,
            Repr::Union(rs) | Repr::Tuple(rs) => rs.iter().any(Repr::is_counted),
            Repr::Record { fields, .. } => fields.iter().any(|(_, r)| r.is_counted()),
            Repr::Nullable(inner) => inner.is_counted(),
            _ => false,
        }
    }

    /// Whether the repr is fully concrete — no `Var` anywhere. After monomorphisation
    /// every reachable repr must satisfy this. `Recursive` is concrete: it is a resolved
    /// back-edge, not an unknown.
    pub fn is_concrete(&self) -> bool {
        match self {
            Repr::Var(_) => false,
            Repr::Record { fields, .. } => fields.iter().all(|(_, r)| r.is_concrete()),
            Repr::Tuple(rs) | Repr::Union(rs) => rs.iter().all(Repr::is_concrete),
            Repr::List(r) | Repr::Nullable(r) => r.is_concrete(),
            Repr::Map(k, v) => k.is_concrete() && v.is_concrete(),
            Repr::Closure { params, ret } => {
                params.iter().all(Repr::is_concrete) && ret.is_concrete()
            }
            _ => true,
        }
    }
}

/// The representation of a type. Total: every `TyId` maps to a `Repr`.
pub fn repr_of(t: &Types, ty: TyId) -> Repr {
    let cyclic = cycle_participants(t, ty);
    let boxed = boxed_atoms(t, ty);
    repr_rec(t, ty, &cyclic, &boxed, true)
}

/// The *pointee* layout of a boxed record — what `repr_of` will not give you, since a
/// value of that type is the pointer.
pub fn repr_shape(t: &Types, atom: u32, boxed: &HashSet<u32>) -> Repr {
    record_repr(t, atom, &HashSet::new(), boxed)
}

/// Every boxed record atom reachable from `ty`, paired with its pointee layout.
pub fn boxed_shapes(t: &Types, ty: TyId, out: &mut HashMap<u32, Repr>) {
    let boxed = boxed_atoms(t, ty);
    for a in &boxed {
        out.entry(*a).or_insert_with(|| repr_shape(t, *a, &boxed));
    }
}

/// Cut the recursion at any type that lies on a cycle, except the root being unfolded.
///
/// The cut set is a property of the type *graph*, not of the walk that reached it. An
/// earlier version cut on "is this type already on the current path", which made the
/// result depend on where the traversal started: `Node | null` reached through `Node`'s
/// field came out as a back-edge, and reached directly as a parameter came out as the
/// expanded union. Two reprs for one type, so the C backend minted two structs for it
/// and then refused to assign one to the other.
///
/// Some root-relative choice is unavoidable — the type graph is cyclic and a finite tree
/// has to reference itself somewhere — so the identity of a recursive type lives in its
/// `TyId`, not in its unfolding. Two reprs denote the same type when they agree up to
/// these back-edges, which is what the backend's type table keys on.
fn repr_rec(
    t: &Types,
    ty: TyId,
    cyclic: &HashSet<TyId>,
    boxed: &HashSet<u32>,
    root: bool,
) -> Repr {
    if !root && cyclic.contains(&ty) {
        return Repr::Recursive(ty);
    }
    repr_components(t, ty, cyclic, boxed)
}

/// Record every cyclic type reachable from `ty`, paired with its unfolding.
///
/// A recursive type has more than one finite unfolding — `Recursive(A)` when the walker
/// reached it from inside `A`, the expanded union when monomorphisation asked for
/// `repr_of(A)` directly — and both are correct. The backend needs this map to see that
/// the two describe one type, and so emit one struct rather than two it then refuses to
/// assign between.
pub fn recursive_unfoldings(t: &Types, ty: TyId, out: &mut HashMap<TyId, Repr>) {
    for u in cycle_participants(t, ty) {
        out.entry(u).or_insert_with(|| repr_of(t, u));
    }
}

/// Every type reachable in one step: the fields, elements, parameters and results of the
/// constructors this type admits. `#0`/`#1`/`#inner` are the generic arguments of the
/// opaque containers and a newtype's payload — real edges — while `#nominal` is only the
/// identity tag and leads nowhere.
fn successors(t: &Types, ty: TyId) -> Vec<TyId> {
    let d = t.data(ty);
    let mut out = Vec::new();
    for atom in positive_atoms(&t.rec_bdd.paths(d.records)) {
        for (n, fty) in &t.rec_atoms[atom as usize].fields {
            let s = t.name_str(*n);
            if !s.starts_with('#') || matches!(s, "#inner" | "#0" | "#1") {
                out.push(*fty);
            }
        }
    }
    for atom in positive_atoms(&t.tup_bdd.paths(d.tuples)) {
        out.extend(t.tup_atoms[atom as usize].elems.iter().copied());
    }
    for atom in positive_atoms(&t.arrow_bdd.paths(d.arrows)) {
        let a = &t.arrow_atoms[atom as usize];
        out.extend(a.params.iter().copied());
        out.push(a.ret);
    }
    out.sort_unstable_by_key(|t| t.0);
    out.dedup();
    out
}

/// The types reachable from `root` that lie on a cycle — Tarjan's strongly connected
/// components, keeping those with more than one member or a self-edge. A type not on a
/// cycle is never cut, so an acyclic program produces exactly the reprs it did before.
fn cycle_participants(t: &Types, root: TyId) -> HashSet<TyId> {
    scc_cycles(&[root], &mut |v| successors(t, v))
}

/// Tarjan's SCC over an arbitrary successor relation, keeping every node that lies on a
/// cycle — a component of more than one node, or a node with an edge to itself.
fn scc_cycles<K: Copy + Eq + std::hash::Hash>(
    roots: &[K],
    succ: &mut dyn FnMut(K) -> Vec<K>,
) -> HashSet<K> {
    struct Scc<'a, K> {
        succ: &'a mut dyn FnMut(K) -> Vec<K>,
        index: HashMap<K, u32>,
        low: HashMap<K, u32>,
        on_stack: HashSet<K>,
        stack: Vec<K>,
        next: u32,
        out: HashSet<K>,
    }
    impl<K: Copy + Eq + std::hash::Hash> Scc<'_, K> {
        fn visit(&mut self, v: K) {
            self.index.insert(v, self.next);
            self.low.insert(v, self.next);
            self.next += 1;
            self.stack.push(v);
            self.on_stack.insert(v);
            for w in (self.succ)(v) {
                if w == v {
                    self.out.insert(v); // a self-edge is a cycle of one
                }
                if !self.index.contains_key(&w) {
                    self.visit(w);
                    let lw = self.low[&w];
                    let lv = self.low[&v];
                    self.low.insert(v, lv.min(lw));
                } else if self.on_stack.contains(&w) {
                    let iw = self.index[&w];
                    let lv = self.low[&v];
                    self.low.insert(v, lv.min(iw));
                }
            }
            if self.low[&v] == self.index[&v] {
                let mut component = Vec::new();
                while let Some(w) = self.stack.pop() {
                    self.on_stack.remove(&w);
                    component.push(w);
                    if w == v {
                        break;
                    }
                }
                if component.len() > 1 {
                    self.out.extend(component);
                }
            }
        }
    }
    let mut s = Scc {
        succ,
        index: HashMap::new(),
        low: HashMap::new(),
        on_stack: HashSet::new(),
        stack: Vec::new(),
        next: 0,
        out: HashSet::new(),
    };
    for &r in roots {
        if !s.index.contains_key(&r) {
            s.visit(r);
        }
    }
    s.out
}


/// The record atoms reachable from `root` that sit on a cycle closing entirely by value,
/// and so have no finite inline layout.
///
/// Keyed by atom, not by type. The type graph cannot answer this: the type system is
/// equi-recursive and hash-consed, so the `TyId` for `Node` is never a successor of
/// anything — `next: Node | null` names the *union's* id. The cycle is a self-loop on that
/// union, and the record actually needing the box sits outside a TyId-keyed search.
///
/// Only records are boxed. A union on the cycle — `Node | null`, `Branch | Leaf` — needs no
/// box of its own: once its record variant is a pointer the union is finite, and boxing it
/// too would bury the discriminant behind an indirection and lose the `null` arm.
fn boxed_atoms(t: &Types, root: TyId) -> HashSet<u32> {
    let mut roots = Vec::new();
    reachable_atoms(t, root, &mut roots, &mut HashSet::new());
    scc_cycles(&roots, &mut |a| atom_value_successors(t, a))
}

/// Every record atom reachable from a type — the search space.
fn reachable_atoms(t: &Types, ty: TyId, out: &mut Vec<u32>, seen: &mut HashSet<TyId>) {
    if !seen.insert(ty) {
        return;
    }
    let d = t.data(ty);
    for a in positive_atoms(&t.rec_bdd.paths(d.records)) {
        if !out.contains(&a) {
            out.push(a);
        }
        for (_, fty) in t.rec_atoms[a as usize].fields.clone() {
            reachable_atoms(t, fty, out, seen);
        }
    }
    for a in positive_atoms(&t.tup_bdd.paths(d.tuples)) {
        for e in t.tup_atoms[a as usize].elems.clone() {
            reachable_atoms(t, e, out, seen);
        }
    }
    for a in positive_atoms(&t.arrow_bdd.paths(d.arrows)) {
        let ar = t.arrow_atoms[a as usize].clone();
        for p in ar.params {
            reachable_atoms(t, p, out, seen);
        }
        reachable_atoms(t, ar.ret, out, seen);
    }
}

/// The record atoms an atom's fields embed *by value*. A `List`/`Map` argument sits behind
/// a pointer and an arrow behind a closure, so neither can make a layout infinite.
fn atom_value_successors(t: &Types, atom: u32) -> Vec<u32> {
    let mut out = Vec::new();
    for (n, fty) in t.rec_atoms[atom as usize].fields.clone() {
        let s = t.name_str(n).to_string();
        if s.starts_with('#') && s != "#inner" {
            continue;
        }
        value_atoms_of(t, fty, &mut out, &mut HashSet::new());
    }
    out
}

/// The record atoms a type embeds by value, through unions and tuples but never through a
/// pointer-backed constructor.
fn value_atoms_of(t: &Types, ty: TyId, out: &mut Vec<u32>, seen: &mut HashSet<TyId>) {
    if !seen.insert(ty) {
        return;
    }
    let d = t.data(ty);
    for a in positive_atoms(&t.rec_bdd.paths(d.records)) {
        if matches!(nominal_name(t, a).as_deref(), Some("List") | Some("Map")) {
            continue;
        }
        if !out.contains(&a) {
            out.push(a);
        }
    }
    for a in positive_atoms(&t.tup_bdd.paths(d.tuples)) {
        for e in t.tup_atoms[a as usize].elems.clone() {
            value_atoms_of(t, e, out, seen);
        }
    }
}

fn repr_components(t: &Types, ty: TyId, cyclic: &HashSet<TyId>, boxed: &HashSet<u32>) -> Repr {
    let d = t.data(ty);

    // `any` is ⊤ — every kind full. It is the one erasure boundary, a boxed value with a
    // type tag; decomposing it into a union of the primitives it happens to admit would
    // both lose the aggregates it must also carry and misrepresent what it is.
    if is_top(t, ty) {
        return Repr::Any;
    }

    let mut comps: Vec<Repr> = Vec::new();

    // Scalar bases. `undef` (the field-absent marker) is not a value and is skipped;
    // if it is the only thing present the type is really `never` for a value.
    if d.base & B_I64 != 0 {
        comps.push(Repr::I64);
    }
    if d.base & B_F64 != 0 {
        comps.push(Repr::F64);
    }
    if d.base & B_STR != 0 {
        comps.push(Repr::Str);
    }
    if d.base & B_BOOL != 0 {
        comps.push(Repr::Bool);
    }
    if d.base & B_NULL != 0 {
        comps.push(Repr::Null);
    }

    // Atoms collapse to one tag: `:ok | :err` is still a single discriminant.
    if !t.atomset_of(d.atoms).is_empty_set() {
        comps.push(Repr::Tag);
    }

    // Rigid variables — abstract until monomorphisation. A union of variables cannot
    // arise from a value, so the first name is enough.
    let vars = t.atomset_of(d.vars);
    if !vars.neg && !vars.names.is_empty() {
        comps.push(Repr::Var(t.name_str(vars.names[0]).to_string()));
    }

    // Each DNF path is a *conjunction* of its positive atoms — an intersection, one record
    // with the fields of all of them — and the set of paths is the disjunction. Flattening
    // the two together would turn `{a} & {b}` into `{a} | {b}`.
    let mut records: Vec<Repr> = Vec::new();
    for (pos, _neg) in t.rec_bdd.paths(d.records) {
        if pos.is_empty() {
            continue;
        }
        // A variant that *is* a boxed record is a pointer to it, not the record inline.
        // The cut in `repr_rec` is keyed by `TyId` and a union's variants are expanded
        // from their record atoms, so without this the boxed record was laid out by value
        // inside the very union its own field points back through.
        let r = match pos.as_slice() {
            [only] if boxed.contains(only) => Repr::BoxedRec(*only),
            _ => record_intersection(t, &pos, cyclic, boxed),
        };
        if !records.contains(&r) {
            records.push(r);
        }
    }
    comps.extend(records);
    for atom in positive_atoms(&t.tup_bdd.paths(d.tuples)) {
        let elems = t.tup_atoms[atom as usize].elems.clone();
        comps.push(if elems.is_empty() {
            Repr::Unit
        } else {
            Repr::Tuple(elems.iter().map(|&e| repr_rec(t, e, cyclic, boxed, false)).collect())
        });
    }
    for atom in positive_atoms(&t.arrow_bdd.paths(d.arrows)) {
        let a = t.arrow_atoms[atom as usize].clone();
        comps.push(Repr::Closure {
            params: a.params.iter().map(|&p| repr_rec(t, p, cyclic, boxed, false)).collect(),
            ret: Box::new(repr_rec(t, a.ret, cyclic, boxed, false)),
        });
    }

    combine(comps)
}

/// Whether a type is ⊤ (`any`): every base bit, every atom, and every record, tuple and
/// arrow admitted.
fn is_top(t: &Types, ty: TyId) -> bool {
    let d = t.data(ty);
    let atoms = t.atomset_of(d.atoms);
    d.base & B_ANY == B_ANY
        && atoms.neg
        && atoms.names.is_empty()
        && d.records == bdd::TRUE
        && d.tuples == bdd::TRUE
        && d.arrows == bdd::TRUE
}

/// Every atom mentioned positively across a type's DNF paths. A union like
/// `Circle | Rect` is stored disjointly — `Circle ∨ (Rect ∧ ¬Circle)` — so a variant
/// can appear in a path that also carries a negation; the negation is only disjointness
/// bookkeeping, and what a value can *be* is the union of the positive mentions.
fn positive_atoms(paths: &[(Vec<u32>, Vec<u32>)]) -> Vec<u32> {
    let mut out: Vec<u32> = paths.iter().flat_map(|(pos, _)| pos.iter().copied()).collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// One DNF path's positive atoms, intersected: a single record carrying every field they
/// require. One atom is the ordinary case — a nominal record, or a variant of a union.
fn record_intersection(t: &Types, atoms: &[u32], cyclic: &HashSet<TyId>, boxed: &HashSet<u32>) -> Repr {
    if let [only] = atoms {
        return record_repr(t, *only, cyclic, boxed);
    }
    let mut fields: Vec<(String, Repr)> = Vec::new();
    let mut name = None;
    for &a in atoms {
        if name.is_none() {
            name = nominal_name(t, a);
        }
        if let Repr::Record { fields: fs, .. } = record_repr(t, a, cyclic, boxed) {
            for (n, r) in fs {
                if !fields.iter().any(|(have, _)| *have == n) {
                    fields.push((n, r));
                }
            }
        }
    }
    Repr::Record { name, fields }
}

fn record_repr(t: &Types, atom_idx: u32, cyclic: &HashSet<TyId>, boxed: &HashSet<u32>) -> Repr {
    let name = nominal_name(t, atom_idx);

    // The runtime containers are opaque nominal records carrying only their generic
    // arguments as `#0`/`#1`.
    match name.as_deref() {
        Some("List") => {
            let elem = field_ty(t, atom_idx, "#0").map_or(Repr::Never, |e| repr_rec(t, e, cyclic, boxed, false));
            return Repr::List(Box::new(elem));
        }
        Some("Map") => {
            let k = field_ty(t, atom_idx, "#0").map_or(Repr::Never, |e| repr_rec(t, e, cyclic, boxed, false));
            let v = field_ty(t, atom_idx, "#1").map_or(Repr::Never, |e| repr_rec(t, e, cyclic, boxed, false));
            return Repr::Map(Box::new(k), Box::new(v));
        }
        _ => {}
    }

    // Real fields only: the `#nominal`/`#0`/… metadata labels are not runtime fields.
    // `#inner` is the exception — it is a newtype's payload, the one thing the wrapper
    // actually holds, and dropping it would leave an empty struct where a value should be.
    let fields: Vec<(NameId, TyId)> = t.rec_atoms[atom_idx as usize]
        .fields
        .iter()
        .filter(|(n, _)| {
            let s = t.name_str(*n);
            !s.starts_with('#') || s == "#inner"
        })
        .copied()
        .collect();
    let fields = fields
        .into_iter()
        .map(|(n, fty)| (t.name_str(n).to_string(), repr_rec(t, fty, cyclic, boxed, false)))
        .collect();
    Repr::Record { name, fields }
}

/// The nominal name of a record atom, read from its reserved `#nominal` field.
fn nominal_name(t: &Types, atom_idx: u32) -> Option<String> {
    let tag = t.rec_atoms[atom_idx as usize].get(t.nominal_label);
    let atoms = t.atomset_of(t.data(tag).atoms);
    (!atoms.neg && atoms.names.len() == 1).then(|| t.name_str(atoms.names[0]).to_string())
}

fn field_ty(t: &Types, atom_idx: u32, name: &str) -> Option<TyId> {
    t.rec_atoms[atom_idx as usize]
        .fields
        .iter()
        .find(|(n, _)| t.name_str(*n) == name)
        .map(|(_, ty)| *ty)
}

/// Fold substituted variants back into a normalised repr. Substituting into a union can
/// change what it *is* — `T | null` with `T = str` is a nullable pointer, not a two-variant
/// union — so an instance must re-normalise or its layout stops matching the one `repr_of`
/// derives for the same type at the call site.
pub fn normalize_union(variants: Vec<Repr>) -> Repr {
    let mut seen: Vec<Repr> = Vec::new();
    for v in variants {
        if !seen.contains(&v) {
            seen.push(v);
        }
    }
    // Canonical order, matching the order `repr_components` pushes them. Without this a
    // substituted `T | null` comes out as `[Null, i64]` — variables are collected after
    // the base bits — while the concrete `i64 | null` is `[i64, Null]`, and the two
    // become different C structs for one type.
    seen.sort_by_key(variant_rank);
    combine(seen)
}

/// Where a variant sits in the canonical union order.
fn variant_rank(r: &Repr) -> u8 {
    match r {
        Repr::I64 => 0,
        Repr::F64 => 1,
        Repr::Str => 2,
        Repr::Bool => 3,
        Repr::Null => 4,
        Repr::Tag => 5,
        Repr::Var(_) => 6,
        Repr::Record { .. } => 7,
        Repr::Unit | Repr::Tuple(_) => 8,
        Repr::Closure { .. } => 9,
        _ => 10,
    }
}

/// Fold the components of a type into one repr. One component is itself; several make a
/// union, with the common `T | null` collapsing to a nullable pointer when it can.
fn combine(mut comps: Vec<Repr>) -> Repr {
    match comps.len() {
        0 => Repr::Never,
        1 => comps.pop().unwrap(),
        _ => {
            // `T | null` with a single pointer-backed `T` is a nullable pointer.
            if comps.len() == 2 {
                let null_at = comps.iter().position(|c| *c == Repr::Null);
                if let Some(i) = null_at {
                    let other = &comps[1 - i];
                    if other.is_pointer() {
                        return Repr::Nullable(Box::new(comps.remove(1 - i)));
                    }
                }
            }
            Repr::Union(comps)
        }
    }
}

#[cfg(test)]
mod tests;
