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

use crate::typecheck::types::{
    NameId, Types, B_BOOL, B_F64, B_I64, B_NULL, B_STR, TyId,
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
    pub fn is_pointer(&self) -> bool {
        matches!(
            self,
            Repr::Str | Repr::List(_) | Repr::Map(_, _) | Repr::Closure { .. } | Repr::Recursive(_)
        )
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
    repr_rec(t, ty, &mut Vec::new())
}

/// `path` is the chain of types currently being expanded. Re-entering one is a
/// recursive (`mu`) back-edge — the recursion is cut with a `Recursive` marker, which
/// is the pointer indirection a recursive type needs to be finite.
fn repr_rec(t: &Types, ty: TyId, path: &mut Vec<TyId>) -> Repr {
    if path.contains(&ty) {
        return Repr::Recursive(ty);
    }
    path.push(ty);
    let r = repr_components(t, ty, path);
    path.pop();
    r
}

fn repr_components(t: &Types, ty: TyId, path: &mut Vec<TyId>) -> Repr {
    let d = t.data(ty);
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

    for atom in positive_atoms(&t.rec_bdd.paths(d.records)) {
        comps.push(record_repr(t, atom, path));
    }
    for atom in positive_atoms(&t.tup_bdd.paths(d.tuples)) {
        let elems = t.tup_atoms[atom as usize].elems.clone();
        comps.push(if elems.is_empty() {
            Repr::Unit
        } else {
            Repr::Tuple(elems.iter().map(|&e| repr_rec(t, e, path)).collect())
        });
    }
    for atom in positive_atoms(&t.arrow_bdd.paths(d.arrows)) {
        let a = t.arrow_atoms[atom as usize].clone();
        comps.push(Repr::Closure {
            params: a.params.iter().map(|&p| repr_rec(t, p, path)).collect(),
            ret: Box::new(repr_rec(t, a.ret, path)),
        });
    }

    combine(comps)
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

fn record_repr(t: &Types, atom_idx: u32, path: &mut Vec<TyId>) -> Repr {
    let name = nominal_name(t, atom_idx);

    // The runtime containers are opaque nominal records carrying only their generic
    // arguments as `#0`/`#1`.
    match name.as_deref() {
        Some("List") => {
            let elem = field_ty(t, atom_idx, "#0").map_or(Repr::Never, |e| repr_rec(t, e, path));
            return Repr::List(Box::new(elem));
        }
        Some("Map") => {
            let k = field_ty(t, atom_idx, "#0").map_or(Repr::Never, |e| repr_rec(t, e, path));
            let v = field_ty(t, atom_idx, "#1").map_or(Repr::Never, |e| repr_rec(t, e, path));
            return Repr::Map(Box::new(k), Box::new(v));
        }
        _ => {}
    }

    // Real fields only: the `#nominal`/`#0`/… metadata labels are not runtime fields.
    let fields: Vec<(NameId, TyId)> = t.rec_atoms[atom_idx as usize]
        .fields
        .iter()
        .filter(|(n, _)| !t.name_str(*n).starts_with('#'))
        .copied()
        .collect();
    let fields = fields
        .into_iter()
        .map(|(n, fty)| (t.name_str(n).to_string(), repr_rec(t, fty, path)))
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
