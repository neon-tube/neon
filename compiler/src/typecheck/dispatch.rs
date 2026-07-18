//! Protocol dispatch. See `docs/design/dispatch.md`.
//!
//! The previous implementation had no answer to *what does a dispatched call
//! return*, so `ir/lower.rs` returned `Erased` from every protocol call except `eq`.
//! That is where the erasure disaster started, and it is why step 7 below — the
//! return is the union of the applicable impls' returns — is the point of this file
//! rather than a detail of it. There is no case where the answer is unknown, so
//! there is nowhere for erasure to enter.

use super::env::{Env, ImplId, ProtocolId};
use super::types::TyId;

/// The decision, recorded so nothing downstream re-resolves it.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    Direct(ImplId),
    /// A switch on the runtime tag with a direct call per arm. Not a vtable: the
    /// applicable set is known right here.
    Switch(Vec<(TyId, ImplId)>),
    /// Inside a generic body, where the receiver is a rigid variable. No impl
    /// applies and none ever will, so the call resolves against the bound in scope
    /// and is discharged at each call site instead.
    Bound { param: String, protocol: ProtocolId },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    pub protocol: ProtocolId,
    pub resolution: Resolution,
    /// The union of the applicable impls' returns — as precise as the receiver is,
    /// and no more.
    pub ret: TyId,
    pub throws: TyId,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DispatchError {
    /// No protocol declares it. The caller checks locals and module fns first.
    UnknownMethod(String),
    /// Two protocols answer. `A::go(r)` picks one.
    Ambiguous { method: String, protocols: Vec<String> },
    /// `S ∧ ¬⋁targetᵢ` is inhabited: some values have no impl, and `uncovered`
    /// names exactly which. A nominal system cannot say this.
    NoImpl { protocol: String, method: String, uncovered: TyId },
    /// `fn make() -> T` — nothing to dispatch on without an expected type.
    NoReceiver(String),
}

/// Resolve `method` called with `args`.
///
/// `qualified` fixes the protocol, which is how `A::go(r)` escapes ambiguity.
/// `expected` is the type the call's result is checked against, and is the
/// dispatch subject for a method with no parameter mentioning the subject.
pub fn resolve(
    env: &mut Env,
    method: &str,
    qualified: Option<ProtocolId>,
    args: &[TyId],
    expected: Option<TyId>,
) -> Result<Selection, DispatchError> {
    let candidates = match qualified {
        Some(p) => vec![p],
        None => env.protocols_with_method(method),
    };
    if candidates.is_empty() {
        return Err(DispatchError::UnknownMethod(method.to_string()));
    }

    // Ambiguity is per protocol, not per impl: two protocols declaring `go` is a
    // question only the caller can settle, and qualification is how.
    let answering: Vec<ProtocolId> = candidates
        .iter()
        .copied()
        .filter(|&p| env.impls_of(p).next().is_some())
        .collect();
    let protocol = match (answering.len(), candidates.len()) {
        (_, 1) => candidates[0],
        (1, _) => answering[0],
        _ => {
            let mut names: Vec<String> = candidates
                .iter()
                .map(|&p| env.protocols()[p.0].name.clone())
                .collect();
            names.sort();
            return Err(DispatchError::Ambiguous { method: method.to_string(), protocols: names });
        }
    };

    // A constructor subject -- `protocol Container for C[_]` -- dispatches by the
    // receiver's head rather than by a subject type, and each method carries its own
    // generics. It is a separate path.
    if env.protocols()[protocol.0].subject_arity > 0 {
        let receiver = args.first().copied().or(expected);
        let Some(receiver) = receiver else {
            return Err(DispatchError::NoReceiver(method.to_string()));
        };
        return hkt_resolve(env, protocol, method, receiver);
    }

    let subject = subject_var(env, protocol);
    let position = dispatch_position(env, protocol, method, subject);

    let receiver = match position {
        Some(i) => args.get(i).copied(),
        // No parameter mentions the subject, so the expected type is all there is.
        None => expected,
    };
    let Some(receiver) = receiver else {
        return Err(DispatchError::NoReceiver(method.to_string()));
    };

    // A rigid receiver is the other resolution path entirely: the body is checked
    // once with `T` opaque, so the bound answers rather than the impl registry.
    if let Some(param) = rigid_name(env, receiver) {
        let (ret, throws) = protocol_method_result(env, protocol, method);
        return Ok(Selection {
            protocol,
            resolution: Resolution::Bound { param, protocol },
            ret,
            throws,
        });
    }

    applicable(env, protocol, method, receiver)
}

/// Dispatch for a constructor-subject protocol. The impl is chosen by matching the
/// receiver's head (`Box[i64]` has head `Box`) against the impl's target head, and
/// the method's own generics are instantiated from the receiver -- so `unwrap(box)`
/// with `box: Box[i64]` returns `i64`, not the method's opaque `T`.
fn hkt_resolve(
    env: &mut Env,
    protocol: ProtocolId,
    method: &str,
    receiver: TyId,
) -> Result<Selection, DispatchError> {
    let name = env.protocols()[protocol.0].name.clone();
    let Some(head) = nominal_head(env, receiver) else {
        return Err(DispatchError::NoImpl {
            protocol: name,
            method: method.to_string(),
            uncovered: receiver,
        });
    };

    let impl_id = env
        .impls_of(protocol)
        .find(|(_, i)| i.target_head.as_deref() == Some(head.as_str()))
        .map(|(id, _)| id);
    let Some(impl_id) = impl_id else {
        return Err(DispatchError::NoImpl { protocol: name, method: method.to_string(), uncovered: receiver });
    };

    // Instantiate the method's generics from the receiver: match its first parameter
    // (`c: Box[T]`) against the receiver (`Box[i64]`) to bind `T`, then substitute.
    let m = env.impls()[impl_id.0].methods.iter().find(|m| m.name == method).cloned();
    let (ret, throws) = match m {
        Some(m) => {
            let var_names: std::collections::HashSet<_> =
                m.generics.iter().map(|g| env.solver.t.name(g)).collect();
            let mut subst = std::collections::HashMap::new();
            if let Some((_, param)) = m.params.first() {
                super::generic::infer(&mut env.solver.t, *param, receiver, &var_names, &mut subst);
            }
            let ret = env.solver.t.substitute(m.ret, &subst);
            let throws = env.solver.t.substitute(m.throws, &subst);
            (ret, throws)
        }
        None => {
            let never = env.solver.t.never();
            (never, never)
        }
    };
    Ok(Selection { protocol, resolution: Resolution::Direct(impl_id), ret, throws })
}

/// The constructor name of a nominal type -- `Box[i64]` → `"Box"` -- read from the
/// reserved `#nominal` atom of its single record atom.
pub(super) fn nominal_head(env: &Env, ty: TyId) -> Option<String> {
    let t = &env.solver.t;
    let d = t.data(ty);
    let atom = match t.rec_bdd.paths(d.records).as_slice() {
        [(pos, neg)] if neg.is_empty() && pos.len() == 1 => &t.rec_atoms[pos[0] as usize],
        _ => return None,
    };
    let tag = atom.get(t.nominal_label);
    let td = t.data(tag);
    let atoms = t.atomset_of(td.atoms);
    (!atoms.neg && atoms.names.len() == 1).then(|| t.name_str(atoms.names[0]).to_string())
}

fn applicable(
    env: &mut Env,
    protocol: ProtocolId,
    method: &str,
    receiver: TyId,
) -> Result<Selection, DispatchError> {
    // An emptiness query per candidate, not a name match.
    let mut hits: Vec<(ImplId, TyId)> = Vec::new();
    let ids: Vec<(ImplId, Option<TyId>)> =
        env.impls_of(protocol).map(|(id, i)| (id, i.target)).collect();
    for (id, target) in ids {
        let Some(target) = target else { continue };
        let meet = env.solver.t.intersect(receiver, target);
        if !env.solver.is_empty(meet) {
            hits.push((id, target));
        }
    }

    let name = env.protocols()[protocol.0].name.clone();
    if hits.is_empty() {
        return Err(DispatchError::NoImpl { protocol: name, method: method.to_string(), uncovered: receiver });
    }

    // Coverage. The residual is a type, so the diagnostic names exactly the values
    // with no impl rather than just the receiver.
    let targets: Vec<TyId> = hits.iter().map(|(_, t)| *t).collect();
    let covered = env.solver.t.union_all(&targets);
    let uncovered = env.solver.t.diff(receiver, covered);
    if !env.solver.is_empty(uncovered) {
        return Err(DispatchError::NoImpl { protocol: name, method: method.to_string(), uncovered });
    }

    let hits = most_specific(env, hits);

    let (ret, throws) = result_of(env, &hits, method);
    let resolution = match hits.as_slice() {
        [(id, target)] if env.solver.is_subtype(receiver, *target) => Resolution::Direct(*id),
        _ => {
            let mut arms: Vec<(TyId, ImplId)> = hits
                .iter()
                .map(|&(id, t)| {
                    let arm = env.solver.t.intersect(receiver, t);
                    (arm, id)
                })
                .collect();
            arms.sort_by_key(|(t, _)| t.0);
            Resolution::Switch(arms)
        }
    };
    Ok(Selection { protocol, resolution, ret, throws })
}

/// Drop any impl strictly less specific than another that also applies.
///
/// decisions.md allows overlap only when nested, so for any value the applicable
/// impls form a chain and a unique minimum exists. That is what makes "most
/// specific" well defined.
fn most_specific(env: &mut Env, hits: Vec<(ImplId, TyId)>) -> Vec<(ImplId, TyId)> {
    let mut out = Vec::new();
    for &(id, t) in &hits {
        let beaten = hits.iter().any(|&(other, u)| {
            other != id && env.solver.is_subtype(u, t) && !env.solver.is_subtype(t, u)
        });
        if !beaten {
            out.push((id, t));
        }
    }
    out
}

/// Step 7, and the whole document: the return is the union over the applicable
/// impls. If they agree it is that type exactly; if they disagree it is a union as
/// imprecise as the receiver and no more. Never erased.
fn result_of(env: &mut Env, hits: &[(ImplId, TyId)], method: &str) -> (TyId, TyId) {
    let mut rets = Vec::new();
    let mut throws = Vec::new();
    for &(id, _) in hits {
        if let Some(m) = env.impls()[id.0].methods.iter().find(|m| m.name == method) {
            rets.push(m.ret);
            throws.push(m.throws);
        }
    }
    let ret = env.solver.t.union_all(&rets);
    let thr = env.solver.t.union_all(&throws);
    (ret, thr)
}

fn protocol_method_result(env: &mut Env, protocol: ProtocolId, method: &str) -> (TyId, TyId) {
    match env.protocols()[protocol.0].methods.iter().find(|m| m.name == method) {
        Some(m) => (m.ret, m.throws),
        None => {
            let n = env.solver.t.never();
            (n, n)
        }
    }
}

/// The protocol's subject as a type. `protocol Area for T` binds `T` in every
/// method signature as a rigid variable, so this is an id comparison.
fn subject_var(env: &mut Env, protocol: ProtocolId) -> TyId {
    let subject = env.protocols()[protocol.0].subject.clone();
    let n = env.solver.t.name(&subject);
    env.solver.t.var(n)
}

/// The first parameter whose declared type is the subject. `None` for
/// `fn make() -> T`, where the expected type is the only candidate.
fn dispatch_position(
    env: &Env,
    protocol: ProtocolId,
    method: &str,
    subject: TyId,
) -> Option<usize> {
    let m = env.protocols()[protocol.0].methods.iter().find(|m| m.name == method)?;
    m.params.iter().position(|(_, t)| *t == subject)
}

/// The receiver's rigid variable name, if it is exactly one.
fn rigid_name(env: &Env, ty: TyId) -> Option<String> {
    let d = env.solver.t.data(ty);
    let vars = env.solver.t.atomset_of(d.vars);
    if vars.neg || vars.names.len() != 1 {
        return None;
    }
    // A union of a variable and something else is not a rigid receiver.
    if d.base != 0 {
        return None;
    }
    Some(env.solver.t.name_str(vars.names[0]).to_string())
}
