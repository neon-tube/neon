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
    /// A `Map[K, V]` — an open-addressed table with control bytes, copied on write
    /// above `rc > 1` (`runtime/src/map.c`). Value semantics at the surface; the
    /// sharing is a runtime property nobody spells.
    Map(Box<Repr>, Box<Repr>),
    /// A refcounted object the runtime owns, carrying whatever generic arguments the
    /// backend needs to see (a payload's element type, so a witness can be emitted for it).
    ///
    /// This is what `@runtime("neon_resource") opaque record Resource[T, E]` produces, and
    /// it carries *both* halves of that declaration under names that cannot be mistaken for
    /// each other:
    ///
    /// - `nominal` is the Neon name (`Resource`) — the type's identity. It is what the
    ///   printed form, the mangled key and the boxed type tag are built from, so
    ///   `x is Resource` on an erased value compares the same string the source wrote.
    /// - `c_type` is the C symbol (`neon_resource`) — a spelling of the pointee, nothing
    ///   more. Only `ctype::c_type` may read it, to emit `{c_type}*`.
    ///
    /// A single `name` field holding the C symbol was the earlier design, and it is
    /// precisely why `type_tag_name` had no Neon name to answer with and every `is` against
    /// a runtime-backed type was false. `c_type` is a pure function of `nominal` (one
    /// lookup in `Types::runtime_types`), so the two can never disagree by construction.
    ///
    /// Replacing one hardcoded `Repr` variant per runtime type is still the point:
    /// refcounting, pointer-ness and substitution are uniform across all of them. Only
    /// equality, ordering and hashing genuinely differ per type, and those live in one
    /// name-keyed table in the C emitter.
    ///
    /// `List` and `Map` are still their own variants; folding them in is mechanical but
    /// their element reprs feed witness emission, so they move separately.
    Runtime { nominal: String, c_type: String, args: Vec<Repr> },
    /// A closure: a function pointer plus a boxed environment. `throws` is part of the
    /// calling convention, not the layout: a throwing closure's function returns the
    /// tagged result `Union([ret, throws])` rather than `ret`, exactly like a named
    /// throwing function. It is a field rather than folded into `ret` because folding
    /// changed the type graph and broke recursive arrow types: the union's struct and its
    /// value-witness resolved the back-edge differently, so the witness emitted `.env` on a
    /// `void*`. A field leaves the type graph identical and combines the two only where the
    /// C signature is built.
    Closure { params: Vec<Repr>, throws: Box<Repr>, ret: Box<Repr> },
    /// A tagged union of two or more distinct variants.
    Union(Vec<Repr>),
    /// `T | null` where `T` is pointer-backed: a nullable pointer, `null` = null pointer.
    Nullable(Box<Repr>),
    /// A rigid type variable, abstract until monomorphisation. A `Var` surviving into a
    /// lowered program is always a substitution someone forgot, and two guards look for
    /// it: `no_type_variable_survives_lowering` in `compiler/tests/ir_lower.rs`, which
    /// scans every value's repr across the lowered corpus and names the offending function
    /// and value, and `c_type`, which panics on a `Var` and so covers every compiled
    /// program rather than only the corpus.
    ///
    /// Note what neither guard is: a check on `repr_of`. `repr_of` returning `Var` for a
    /// type that still mentions a variable is correct and expected — that is what the
    /// variant is for. The bugs were all downstream, in lowering calling `repr_of` where
    /// it should have called `repr_of_ty`, so the test runs over the lowered IR instead.
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
    /// THE BLOCK-PARAMETER RELATION: a predecessor may pass an
    /// argument of repr `self` to a parameter of repr `target` iff this holds. It is
    /// not equality — the emitter widens at flow sites, so `str` and `Null` both flow
    /// into a `str?` join and a bare `i64` into an `i64 | null` parameter — and until
    /// this function existed the invariant those 9,000+ sites satisfied was written
    /// nowhere. This is the definition; `ir_lower.rs`'s
    /// `block_arguments_are_assignable_to_their_parameters` is the verifier, so a
    /// lowering change that starts passing an inconvertible value fails a test
    /// instead of reading garbage at the join.
    ///
    /// The relation is exactly the set of conversions `backend::c::coerce_expr` emits
    /// without falling through to its zeroed dead-branch literal, in the WIDENING
    /// direction: identity, `never` into anything, anything into `any`, injection or
    /// remap into a union, `null`/payload into a nullable, and covariant width on
    /// records and tuples (an absent record field must be optional — the emitter
    /// fills it with null). Narrowing conversions exist in the backend too (a
    /// refined variable's projection), but a block argument is a flow, and flows
    /// widen.
    pub fn assignable(&self, target: &Repr) -> bool {
        if self == target || matches!(self, Repr::Never) || matches!(target, Repr::Any) {
            return true;
        }
        match (self, target) {
            (src, Repr::Union(vs)) => {
                vs.iter().any(|v| v == src)
                    || matches!(src, Repr::Union(from) if from.iter().any(|f| vs.contains(f)))
            }
            // A union whose other variants are all `Never` is its one inhabited member
            // wearing a tag: `try`'s machinery types an error edge at `E | never`, and
            // the projection to `E` at the join is total. Found by the verifier on its
            // first run — this arm is the definition learning what the emitter
            // actually does, which is the whole point of writing it down.
            (Repr::Union(from), t) => {
                from.iter().all(|f| matches!(f, Repr::Never) || f == t)
                    && from.iter().any(|f| f == t)
            }
            (Repr::Null, Repr::Nullable(_)) => true,
            (src, Repr::Nullable(inner)) => src == inner.as_ref(),
            (Repr::Record { fields: sf, .. }, Repr::Record { fields: tf, .. }) => {
                tf.iter().all(|(n, tr)| match sf.iter().find(|(sn, _)| sn == n) {
                    Some((_, sr)) => sr.assignable(tr),
                    None => Repr::Null.assignable(tr),
                })
            }
            (Repr::Tuple(se), Repr::Tuple(te)) => {
                te.len() <= se.len() && se.iter().zip(te).all(|(s, t)| s.assignable(t))
            }
            _ => false,
        }
    }

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
                | Repr::Runtime { .. }
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
            | Repr::Runtime { .. }
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
            Repr::Runtime { args, .. } => args.iter().all(Repr::is_concrete),
            Repr::Closure { params, throws, ret } => {
                params.iter().all(Repr::is_concrete) && throws.is_concrete() && ret.is_concrete()
            }
            _ => true,
        }
    }
}

/// The representation of a type. Total: every `TyId` maps to a `Repr`, and the
/// no-component case comes out as `Never` rather than as an absence a caller has to guess
/// at — the absence is what the graveyard's `Erased` fallback grew out of.
///
/// Both cut sets are recomputed here on every call: two SCC passes over the whole type
/// graph reachable from `ty`. They have to be, because both are properties of the graph
/// rooted at this type rather than of the walk, and the results are identical for a given
/// `ty` — so this is safe to call repeatedly, just not cheap in a hot loop.
pub fn repr_of(t: &Types, ty: TyId) -> Repr {
    let cyclic = cycle_participants(t, ty);
    let boxed = boxed_atoms(t, ty);
    repr_rec(t, ty, &cyclic, &boxed, true)
}

/// The *pointee* layout of a boxed record — what `repr_of` will not give you, since a
/// value of that type is the pointer.
///
/// The cut set is computed over `layout_successors` — the walk this function actually
/// performs — rather than over `successors`, and that is the whole point: boxing already
/// cuts every cycle that closes through a record field, so a cut set built from the full
/// type graph would insert back-edges where the expansion terminates anyway and change
/// every existing boxed record's pointee layout. Cutting the layout walk's *own* cycles
/// adds a back-edge in exactly the cases that had none.
///
/// An empty cut set was the earlier design, on the reasoning that boxing cuts everything.
/// It cuts every *record* cycle. A boxed record reaching a pointer-backed cycle — a
/// `List[L]` inside an `L` — has a layout walk boxing does not cut, and the compiler
/// recursed until the stack ran out. See `tests/lang/records/boxed_record_reaching_a_pointer_cycle.neon`.
pub fn repr_shape(t: &Types, atom: u32, boxed: &HashSet<u32>) -> Repr {
    let roots: Vec<TyId> = t.rec_atoms[atom as usize].fields.iter().map(|(_, ty)| *ty).collect();
    let cyclic = scc_cycles(&roots, &mut |v| layout_successors(t, v, boxed));
    record_repr(t, atom, &cyclic, boxed)
}

/// The types the inline layout walk descends into for `ty` — `repr_components`' recursive
/// calls, edge for edge, including the stop at a boxed record.
///
/// This mirrors `repr_components`/`record_repr` rather than `successors`: `successors`
/// describes the *type* graph, which follows a boxed record's fields, while the layout
/// walk stops dead at one. Keeping the two apart is what lets `repr_shape` cut only the
/// cycles that would not otherwise terminate.
fn layout_successors(t: &Types, ty: TyId, boxed: &HashSet<u32>) -> Vec<TyId> {
    if is_top(t, ty) {
        return Vec::new();
    }
    let d = t.data(ty);
    let mut out = Vec::new();
    for (pos, _neg) in t.rec_bdd.paths(d.records) {
        // A single boxed atom becomes a `BoxedRec` pointer and the walk stops.
        if pos.is_empty() || matches!(pos.as_slice(), [only] if boxed.contains(only)) {
            continue;
        }
        for &a in &pos {
            atom_layout_successors(t, a, &mut out);
        }
    }
    for atom in positive_atoms(&t.tup_bdd.paths(d.tuples)) {
        out.extend(t.tup_atoms[atom as usize].elems.iter().copied());
    }
    for atom in positive_atoms(&t.arrow_bdd.paths(d.arrows)) {
        let a = &t.arrow_atoms[atom as usize];
        out.extend(a.params.iter().copied());
        out.push(a.throws);
        out.push(a.ret);
    }
    out.sort_unstable_by_key(|t| t.0);
    out.dedup();
    out
}

/// The types `record_repr` descends into for one atom: a `@runtime` type's generic slots,
/// a `List`/`Map`'s element slots, or an ordinary record's real fields.
fn atom_layout_successors(t: &Types, atom: u32, out: &mut Vec<TyId>) {
    let name = nominal_name(t, atom);
    if name.as_deref().is_some_and(|n| t.runtime_types.contains_key(n)) {
        out.extend((0..).map_while(|i| field_ty(t, atom, &format!("#{i}"))));
        return;
    }
    match name.as_deref() {
        Some("std::collections::list::List") => out.extend(field_ty(t, atom, "#0")),
        Some("std::collections::map::Map") => {
            out.extend(field_ty(t, atom, "#0"));
            out.extend(field_ty(t, atom, "#1"));
        }
        _ => {
            for (n, fty) in &t.rec_atoms[atom as usize].fields {
                let s = t.name_str(*n);
                if !s.starts_with('#') || s == "#inner" {
                    out.push(*fty);
                }
            }
        }
    }
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

/// Every type reachable in one step: the fields, elements, parameters, error clauses and
/// results of the constructors this type admits. `#0`/`#1`/`#inner` are the generic
/// arguments of the opaque containers and a newtype's payload — real edges — while
/// `#nominal` is only the identity tag and leads nowhere.
///
/// An arrow contributes all three of `params`, `throws` and `ret`, because that is what
/// `repr_components` descends into when it builds the `Closure`. Omitting `throws` here
/// was two lists of an arrow's parts kept in step by hand, and they drifted:
/// `mu type F = null | (i64) throws F -> i64` is contractive and the checker accepts it,
/// but its cycle closes through the clause, so `cycle_participants` did not see it and
/// `repr_of` recursed until the stack ran out. See
/// `tests/lang/types/mu_type_through_arrow_throws_clause.neon`.
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
        out.push(a.throws);
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
        // `throws` as much as `ret`: a record atom mentioned only in an error clause is
        // still reachable, and `boxed_atoms` searches only what this collects.
        reachable_atoms(t, ar.throws, out, seen);
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
        if matches!(nominal_name(t, a).as_deref(), Some("std::collections::list::List") | Some("std::collections::map::Map")) {
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

/// Decompose a type into the reprs its constructors admit, then `combine` them.
///
/// The push order — scalars, tag, variable, records, tuples, arrows — is not incidental:
/// it defines the canonical variant order of a union, and `variant_rank` exists only to
/// reproduce it for reprs that arrive by substitution rather than from a `TyId`. Two
/// orderings for one type mean two C structs the backend then refuses to assign between,
/// so a new component belongs in both places at once.
///
/// This runs *below* the recursion cut: it calls `repr_rec` for every nested type, which
/// is where a cyclic type becomes a back-edge. Calling `repr_of` for a nested type instead
/// would restart the cut computation from a different root and produce a second, unequal
/// repr for the same type.
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
            throws: Box::new(repr_rec(t, a.throws, cyclic, boxed, false)),
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

/// The layout of one record atom, in three tiers checked in order: a `@runtime`-backed
/// type, then the two built-in containers, then an ordinary record laid out by its fields.
///
/// All three tiers dispatch on the atom's `#nominal` name. The first is table-driven —
/// adding a runtime-backed type is a stdlib declaration, not a compiler edit — while
/// `List` and `Map` are matched by literal name, so those two names are load-bearing here
/// in a way no other type's is.
///
/// This never returns `BoxedRec`. Boxing is decided one level up, in `repr_components`,
/// where the union path knows whether the atom is on a by-value cycle; here the atom is
/// always laid out inline, which is exactly what `repr_shape` wants when it asks for a
/// boxed record's pointee.
fn record_repr(t: &Types, atom_idx: u32, cyclic: &HashSet<TyId>, boxed: &HashSet<u32>) -> Repr {
    let name = nominal_name(t, atom_idx);

    // A record the runtime owns: `@runtime("neon_file")` names the C type, and the
    // record's generic arguments — the `#0`, `#1` slots — become the repr's arguments, so
    // a payload's element type reaches the backend and can get a witness.
    //
    // A new runtime-backed type is a stdlib declaration, not a compiler edit.
    if let Some((nominal, sym)) = name.as_deref().and_then(|n| Some((n, t.runtime_types.get(n)?)))
    {
        let args = (0..)
            .map_while(|i| field_ty(t, atom_idx, &format!("#{i}")))
            .map(|a| repr_rec(t, a, cyclic, boxed, false))
            .collect();
        // Both halves are in hand at exactly this point: the table is keyed by the Neon
        // name and holds the C symbol. Carrying only the symbol here is what made
        // `x is Resource` unanswerable downstream.
        return Repr::Runtime { nominal: nominal.to_string(), c_type: sym.clone(), args };
    }

    // `List` and `Map` still have their own reprs: their element types drive witness
    // emission and the codegen-assisted natives, so they move separately.
    match name.as_deref() {
        Some("std::collections::list::List") => {
            let elem = field_ty(t, atom_idx, "#0").map_or(Repr::Never, |e| repr_rec(t, e, cyclic, boxed, false));
            return Repr::List(Box::new(elem));
        }
        Some("std::collections::map::Map") => {
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

/// One field of a record atom by literal name, for reading the reserved `#`-prefixed slots
/// (`#0`, `#1`, `#inner`) that carry generic arguments and a newtype's payload.
///
/// `None` means the slot is absent, and callers treat that as `Repr::Never` rather than as
/// an error — an uninstantiated `List` has no `#0`. `record_repr` also relies on the
/// generic slots being numbered contiguously from `#0`, since it collects them with
/// `map_while` and stops at the first gap.
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

/// Where a variant sits in the canonical union order — the order `repr_components` pushes
/// its components in, reproduced for reprs that arrive by substitution rather than from a
/// `TyId`. The two are one specification in two places, so they are written to be read
/// side by side: scalars, tag, variable, *everything the records phase emits*, tuples,
/// arrows.
///
/// `List`, `Map`, `Runtime` and `BoxedRec` all belong to rank 7. They do not look like
/// records, but every one of them is produced by `repr_components`' record loop — they are
/// what a record atom becomes when its `#nominal` names a container or a `@runtime` type —
/// so they are pushed *before* tuples and arrows. Sorting them after, as a catch-all rank
/// did, made `List[i64] | (i64, str)` come out `[List, Tuple]` from `repr_of` and
/// `[Tuple, List]` from `normalize_union`: one type, two variant orders, two C structs the
/// backend then refuses to assign between.
///
/// Ranks 0–9 are therefore exhaustive over what can actually reach here. The trailing arm
/// covers `Nullable`, `Recursive`, `Any` and `Never`, none of which `repr_components`
/// pushes as a union component — `Nullable` is made by `combine` *after* the ordering is
/// fixed, and the others only ever appear nested inside a variant.
fn variant_rank(r: &Repr) -> u8 {
    match r {
        Repr::I64 => 0,
        Repr::F64 => 1,
        Repr::Str => 2,
        Repr::Bool => 3,
        Repr::Null => 4,
        Repr::Tag => 5,
        Repr::Var(_) => 6,
        Repr::Record { .. }
        | Repr::List(_)
        | Repr::Map(_, _)
        | Repr::Runtime { .. }
        | Repr::BoxedRec(_) => 7,
        Repr::Unit | Repr::Tuple(_) => 8,
        Repr::Closure { .. } => 9,
        // None of these is pushed as a union component by `repr_components`. `Nullable` is
        // made by `combine` after the ordering is fixed; `Recursive`, `Any` and `Never`
        // only appear nested inside a variant.
        //
        // `Union` is the one that can genuinely turn up, when a variable standing for a
        // union type is substituted into an enclosing one — and a rank cannot repair it.
        // The type system flattens `(a | b) | c`, so `repr_of` never produces the nested
        // shape at all; ordering a nested union against its siblings would still be a
        // different repr from the flat one. Flattening belongs in `normalize_union`, not
        // here, and is left unfixed rather than papered over. See the audit notes.
        Repr::Union(_) | Repr::Nullable(_) | Repr::Recursive(_) | Repr::Any | Repr::Never => 10,
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
