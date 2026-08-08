//! Protocol dispatch. See `docs/design/dispatch.md`.
//!
//! The previous implementation had no answer to *what does a dispatched call
//! return*, so `ir/lower.rs` returned `Erased` from every protocol call except `eq`.
//! That is where the erasure disaster started, and it is why step 7 below — the
//! return is the union of the applicable impls' returns — is the point of this file
//! rather than a detail of it. There is no case where the answer is unknown, so
//! there is nowhere for erasure to enter.

use super::env::{Env, ImplId, ProtocolId};
use super::types::{NameId, TyId};
use std::collections::{HashMap, HashSet};

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
    ///
    /// `subject_pos` is which argument carried the subject, and `None` means the
    /// subject is the RETURN — `fn from_json(j: Json) -> T`, dispatched on the type the
    /// call is checked against. Lowering needs it to know where to look for the head:
    /// reading `args[0]` regardless is how a return-position call came to be discharged
    /// against its unrelated first argument, which finds the wrong impl whenever that
    /// argument's type happens to have one.
    Bound {
        param: String,
        protocol: ProtocolId,
        subject_pos: Option<usize>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    pub protocol: ProtocolId,
    pub resolution: Resolution,
    /// The union of the applicable impls' returns — as precise as the receiver is,
    /// and no more.
    pub ret: TyId,
    pub throws: TyId,
    /// Which argument the dispatch was decided on, when one was. The checker's opacity
    /// gate needs it: an impl whose target is a structural type is a structural view of
    /// whatever flows into it, and the receiver is the value that flows.
    pub receiver_pos: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DispatchError {
    /// No protocol declares it. The caller checks locals and module fns first.
    UnknownMethod(String),
    /// Two protocols answer. `A::go(r)` picks one.
    Ambiguous {
        method: String,
        protocols: Vec<String>,
    },
    /// `S ∧ ¬⋁targetᵢ` is inhabited: some values have no impl, and `uncovered`
    /// names exactly which. A nominal system cannot say this.
    NoImpl {
        protocol: String,
        method: String,
        uncovered: TyId,
    },
    /// `fn make() -> T` — nothing to dispatch on without an expected type.
    NoReceiver(String),
    /// The subject is the return and it is a union needing a different impl per variant.
    /// There is no value in hand whose tag could choose one.
    NoSubjectToSwitch {
        protocol: String,
        method: String,
        subject: TyId,
    },
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
            return Err(DispatchError::Ambiguous {
                method: method.to_string(),
                protocols: names,
            });
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
        return hkt_resolve(env, protocol, method, args, receiver);
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
        let (ret, throws) = protocol_method_result(env, protocol, method, &param);
        return Ok(Selection {
            protocol,
            resolution: Resolution::Bound {
                param,
                protocol,
                subject_pos: position,
            },
            ret,
            throws,
            receiver_pos: position,
        });
    }

    applicable(env, protocol, method, receiver, position)
}

/// Dispatch for a constructor-subject protocol. The impl is chosen by matching the
/// receiver's head (`Box[i64]` has head `Box`) against the impl's target head, and
/// the method's own generics are instantiated from the receiver -- so `unwrap(box)`
/// with `box: Box[i64]` returns `i64`, not the method's opaque `T`.
fn hkt_resolve(
    env: &mut Env,
    protocol: ProtocolId,
    method: &str,
    args: &[TyId],
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
        return Err(DispatchError::NoImpl {
            protocol: name,
            method: method.to_string(),
            uncovered: receiver,
        });
    };

    // Instantiate the method's generics from the arguments: match each parameter
    // (`c: Box[T]`, `init: A`) against its argument to bind `T`, `A`, then substitute.
    // The receiver alone is not enough -- `fold`'s accumulator `A` comes from `init`,
    // not from the container -- so every argument feeds the inference.
    let m = env.impls()[impl_id.0]
        .methods
        .iter()
        .find(|m| m.name == method)
        .cloned();
    let (ret, throws) = match m {
        Some(m) => {
            let var_names: std::collections::HashSet<_> =
                m.generics.iter().map(|g| env.solver.t.name(g)).collect();
            let mut subst = std::collections::HashMap::new();
            for ((_, param), arg) in m.params.iter().zip(args) {
                super::generic::infer(&mut env.solver.t, *param, *arg, &var_names, &mut subst);
            }
            let ret = env.solver.t.substitute(m.ret, &subst);
            let throws = env.solver.t.substitute(m.throws, &subst);
            (ret, throws)
        }
        // The impl relies on the protocol's default body, so the protocol's own signature
        // is the answer. Falling through to `never` here said "this call produces a value
        // nothing inhabits" with no diagnostic — see `result_of` for the same shape.
        None => match env.protocols()[protocol.0]
            .methods
            .iter()
            .find(|m| m.name == method)
        {
            Some(m) => (m.ret, m.throws),
            None => {
                let never = env.solver.t.never();
                (never, never)
            }
        },
    };
    let receiver_pos = if args.is_empty() { None } else { Some(0) };
    Ok(Selection {
        protocol,
        resolution: Resolution::Direct(impl_id),
        ret,
        throws,
        receiver_pos,
    })
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

/// A candidate impl, with the substitution its head needed in order to name the
/// receiver at all. The substitution is empty for a non-generic impl, which is what
/// every impl was until this file learned to match a generic head.
struct Hit {
    id: ImplId,
    /// The impl's target *under* `subst`: `impl[T] P for Pair[T]` matched against
    /// `Pair[i64]` has head `Pair[i64]`, not `Pair[T]`. Everything downstream —
    /// coverage, specificity, the switch arms — reasons about this and never about
    /// the written target, because the written target is not a type the receiver can
    /// be compared with.
    head: TyId,
    subst: HashMap<NameId, TyId>,
}

fn applicable(
    env: &mut Env,
    protocol: ProtocolId,
    method: &str,
    receiver: TyId,
    receiver_pos: Option<usize>,
) -> Result<Selection, DispatchError> {
    // An emptiness query per candidate, not a name match.
    let mut hits: Vec<Hit> = Vec::new();
    for c in candidates(env, protocol) {
        let Some(target) = c.target else { continue };
        let Some((head, subst)) = match_head(env, target, &c.generics, receiver) else {
            continue;
        };
        if !bounds_hold(env, &c, &subst, &mut Vec::new()) {
            continue;
        }
        let meet = env.solver.t.intersect(receiver, head);
        if !env.solver.is_empty(meet) {
            hits.push(Hit {
                id: c.id,
                head,
                subst,
            });
        }
    }

    let name = env.protocols()[protocol.0].name.clone();
    if hits.is_empty() {
        return Err(DispatchError::NoImpl {
            protocol: name,
            method: method.to_string(),
            uncovered: receiver,
        });
    }

    // Coverage. The residual is a type, so the diagnostic names exactly the values
    // with no impl rather than just the receiver.
    let targets: Vec<TyId> = hits.iter().map(|h| h.head).collect();
    let covered = env.solver.t.union_all(&targets);
    let uncovered = env.solver.t.diff(receiver, covered);
    if !env.solver.is_empty(uncovered) {
        return Err(DispatchError::NoImpl {
            protocol: name,
            method: method.to_string(),
            uncovered,
        });
    }

    // Specificity, except in return position, where an impl covering the WHOLE subject is
    // the only thing that can answer.
    //
    // `most_specific` prefers the narrower heads, which is right when there is a value to
    // test: `impl P for i64` beside `impl P for i64 | str` should win for an i64 argument.
    // With the subject in the return there is nothing to test, so preferring the narrow ones
    // leaves a switch that cannot be lowered — and drops the one impl that could have
    // answered. `impl FromJson for i64 | str`, written precisely to decide the arm itself,
    // was discarded in favour of the two impls that need a tag nobody has.
    let covering = if receiver_pos.is_none() {
        hits.iter()
            .position(|h| env.solver.is_subtype(receiver, h.head))
    } else {
        None
    };
    let hits = match covering {
        Some(i) => {
            let mut hits = hits;
            vec![hits.swap_remove(i)]
        }
        None => most_specific(env, hits),
    };

    let (ret, throws) = result_of(env, &hits, method, protocol);
    let resolution = match hits.as_slice() {
        [h] if env.solver.is_subtype(receiver, h.head) => Resolution::Direct(h.id),
        _ if receiver_pos.is_none() => {
            // A switch tests the receiver's runtime tag to pick an arm. When the subject is
            // the RETURN there is no receiver — nothing exists yet whose tag could be read —
            // so there is no way to choose, and this has to be refused rather than lowered.
            //
            // Lowering it switched on `args[0]` instead, which for `from_json(j: Json) -> T`
            // is the document: it compared the document's tag against the SUBJECT's variants,
            // rebuilt an argument from the projected payload, and gave the last arm no test at
            // all — so `dec(true)` on an `i64 | str` read a bool payload as a `neon_str` and
            // segfaulted. Same shape as the `Bound` path's `args[0]` assumption, in the other
            // resolution.
            //
            // Choosing an arm from the *document* is a real thing to want and it is not this
            // layer's to do: only the protocol knows what shapes each impl accepts, so the
            // check that two arms cannot both match is a fact about JSON, not about dispatch.
            // Write an impl for the union that inspects the value and decides.
            return Err(DispatchError::NoSubjectToSwitch {
                protocol: name,
                method: method.to_string(),
                subject: receiver,
            });
        }
        _ => {
            let mut arms: Vec<(TyId, ImplId)> = hits
                .iter()
                .map(|h| {
                    let arm = env.solver.t.intersect(receiver, h.head);
                    (arm, h.id)
                })
                .collect();
            arms.sort_by_key(|(t, _)| t.0);
            Resolution::Switch(arms)
        }
    };
    Ok(Selection {
        protocol,
        resolution,
        ret,
        throws,
        receiver_pos,
    })
}

/// Match an impl's head against the receiver, treating the impl's *own* generics as
/// holes to solve rather than as the rigid variables they are inside its body.
///
/// This is what makes a generic impl fire. `impl[T] Tag for Pair[T]` declares `T`
/// rigid, so `Pair[T] ∧ Pair[i64]` is empty and the meet test alone rejected the impl
/// for every receiver in the language: the feature parsed, checked its own body, and
/// never applied to anything. Matching instead binds `T := i64` and yields the head
/// `Pair[i64]`, which the caller's emptiness test then judges exactly as it judges a
/// monomorphic impl's target.
///
/// The match only *proposes* a substitution; applicability is still decided by that
/// emptiness test. Structure can line up by accident — two unrelated nominals both
/// carrying a `#0` field will bind a variable from each other — and the proposal is
/// then a head the receiver does not meet, so it is rejected there.
///
/// `None` declines the impl. A generic left unbound is the honest case to decline:
/// the head would still mention a variable, which is neither a type the receiver can
/// be compared against nor a layout an instance can be lowered at. The receiver is
/// the only thing consulted, so every binding this produces is one lowering can
/// re-derive from the receiver's own repr at the call site.
/// One impl of a protocol, read out of the registry before the search borrows `env`
/// mutably. `wheres` and `module` travel together because a bound is written as a path
/// and resolves in the module that wrote it.
struct Candidate {
    id: ImplId,
    target: Option<TyId>,
    generics: Vec<String>,
    wheres: Vec<(String, Vec<String>)>,
    module: Vec<String>,
}

fn candidates(env: &Env, protocol: ProtocolId) -> Vec<Candidate> {
    env.impls_of(protocol)
        .map(|(id, i)| Candidate {
            id,
            target: i.target,
            generics: i.generics.clone(),
            wheres: i.wheres.clone(),
            module: i.module.clone(),
        })
        .collect()
}

/// Discharge an impl's `where` clauses under the substitution its head produced.
/// `impl[T] Serialize for List[T] where T: Serialize` matched against `List[i64]` binds
/// `T := i64` and asks whether `i64` serializes; if it does not, this impl is not the
/// one for `List[i64]`, and the caller declines it.
///
/// A failed bound *declines* rather than errors, which is what makes the diagnostic the
/// right one: coverage is then computed over the impls that really do apply, so the
/// error names the values left uncovered instead of an impl the author never meant.
///
/// A binding that is still a variable is not this call's obligation. Inside a generic
/// body the receiver can be `Pair[T]` for the *caller's* rigid `T`, and whether that `T`
/// serializes is settled by the caller's own bound, where the caller is called.
fn bounds_hold(
    env: &mut Env,
    c: &Candidate,
    subst: &HashMap<NameId, TyId>,
    seen: &mut Vec<(ProtocolId, TyId)>,
) -> bool {
    for (param, path) in &c.wheres {
        let n = env.solver.t.name(param);
        let Some(&bound_to) = subst.get(&n) else {
            continue;
        };
        if env.is_error(bound_to) || super::generic::is_var(&env.solver.t, bound_to) {
            continue;
        }
        let Some(pid) = env.lookup_protocol(&c.module, path) else {
            continue;
        };
        if !satisfies(env, pid, bound_to, seen) {
            return false;
        }
    }
    true
}

/// Whether some impl of `protocol` covers `ty` — the subgoal a `where` bound raises.
///
/// This is `applicable`'s candidate loop with the method question removed, and it is a
/// separate query rather than `Env::type_satisfies` because that one compares against
/// the impls' *written* targets: a rigid `T` again, so it answers no for every generic
/// impl, which is the same defect one level down. Markers and constructor-subject
/// protocols have no impl head to match and are still its business.
///
/// `seen` assumes a goal already being proved. A recursive type asks its own question
/// back — a `mu Tree = List[Tree]` needs `Serialize for Tree` in order to answer
/// `Serialize for Tree` — and failing there would reject every recursive type, while
/// the instance it stands for is finite: types are hash-consed, so the monomorphised
/// body closes on itself rather than expanding forever. Otherwise this terminates
/// because each subgoal is structurally smaller than the goal that raised it.
fn satisfies(
    env: &mut Env,
    protocol: ProtocolId,
    ty: TyId,
    seen: &mut Vec<(ProtocolId, TyId)>,
) -> bool {
    if env.protocols()[protocol.0].is_marker || env.protocols()[protocol.0].subject_arity > 0 {
        return env.type_satisfies(ty, protocol);
    }
    if seen.contains(&(protocol, ty)) {
        return true;
    }
    seen.push((protocol, ty));
    let mut heads = Vec::new();
    for c in candidates(env, protocol) {
        let Some(target) = c.target else { continue };
        let Some((head, subst)) = match_head(env, target, &c.generics, ty) else {
            continue;
        };
        if bounds_hold(env, &c, &subst, seen) {
            heads.push(head);
        }
    }
    seen.pop();
    if heads.is_empty() {
        return false;
    }
    // A subtype of the *union*, not of any one head, so a union type is covered when its
    // arms are covered between them — the same rule `type_satisfies` states.
    let covered = env.solver.t.union_all(&heads);
    env.solver.is_subtype(ty, covered)
}

fn match_head(
    env: &mut Env,
    target: TyId,
    generics: &[String],
    receiver: TyId,
) -> Option<(TyId, HashMap<NameId, TyId>)> {
    if generics.is_empty() {
        return Some((target, HashMap::new()));
    }
    let vars: HashSet<NameId> = generics.iter().map(|g| env.solver.t.name(g)).collect();
    let mut subst = HashMap::new();
    super::generic::infer(&mut env.solver.t, target, receiver, &vars, &mut subst);
    if vars.iter().any(|v| !subst.contains_key(v)) {
        return None;
    }
    let head = env.solver.t.substitute(target, &subst);
    Some((head, subst))
}

/// Drop any impl strictly less specific than another that also applies.
///
/// decisions.md allows overlap only when nested, so for any value the applicable
/// impls form a chain and a unique minimum exists. That is what makes "most
/// specific" well defined.
fn most_specific(env: &mut Env, hits: Vec<Hit>) -> Vec<Hit> {
    let mut out = Vec::new();
    for h in &hits {
        let beaten = hits.iter().any(|o| {
            o.id != h.id
                && env.solver.is_subtype(o.head, h.head)
                && !env.solver.is_subtype(h.head, o.head)
        });
        if !beaten {
            out.push(Hit {
                id: h.id,
                head: h.head,
                subst: h.subst.clone(),
            });
        }
    }
    out
}

/// Step 7, and the whole document: the return is the union over the applicable
/// impls. If they agree it is that type exactly; if they disagree it is a union as
/// imprecise as the receiver and no more. Never erased.
///
/// An impl that does not carry the method is one relying on the protocol's *default*
/// body, so the protocol's own signature answers for it. Contributing nothing used to
/// mean the union was taken over an empty set, which is `never` — a type no value
/// inhabits, handed to lowering as the call's result with no diagnostic anywhere. The
/// case is currently unreachable, because a protocol method with a body does not
/// typecheck at all (`check.rs` checks it with the subject unbound, so its `T` parameter
/// is an unknown type), but the silent `never` was one fix away from being live.
fn result_of(env: &mut Env, hits: &[Hit], method: &str, protocol: ProtocolId) -> (TyId, TyId) {
    let mut rets = Vec::new();
    let mut throws = Vec::new();
    for h in hits {
        let found = env.impls()[h.id.0]
            .methods
            .iter()
            .find(|m| m.name == method);
        let sig = match found {
            Some(m) => Some((m.ret, m.throws)),
            None => env.protocols()[protocol.0]
                .methods
                .iter()
                .find(|m| m.name == method)
                .map(|m| (m.ret, m.throws)),
        };
        // Under the impl's own substitution: `impl[T] Get for Box[T]` declares
        // `fn get(b: Box[T]) -> T`, whose answer for a `Box[i64]` receiver is `i64`.
        // Reading the written `T` here would hand lowering a rigid variable as the
        // call's result type.
        if let Some((ret, thr)) = sig {
            rets.push(env.solver.t.substitute(ret, &h.subst));
            throws.push(env.solver.t.substitute(thr, &h.subst));
        }
    }
    let ret = env.solver.t.union_all(&rets);
    let thr = env.solver.t.union_all(&throws);
    (ret, thr)
}

/// The protocol's declared result for `method`, with the protocol's own subject variable
/// rewritten to the rigid variable the bound is in scope for.
///
/// The substitution is the whole of it. `protocol FromJson for T { fn from_json(j: Json) -> T }`
/// declares its return as the subject `T`, and inside `impl[V] FromJson for Map[str, V]` the
/// call's result is a `V`, not a `T`. Returning the declaration verbatim made that a
/// mismatch — "expected `V`, found `T`" — and only ever type-checked in a body whose own
/// parameter happened to be spelled `T` too, which is why the `List[T]` impl next door
/// looked fine.
///
/// It stays invisible for a protocol whose result does not mention the subject: `ToJson`
/// returns `Json` either way, which is why the encode side never needed this.
fn protocol_method_result(
    env: &mut Env,
    protocol: ProtocolId,
    method: &str,
    param: &str,
) -> (TyId, TyId) {
    let found = env.protocols()[protocol.0]
        .methods
        .iter()
        .find(|m| m.name == method);
    let Some((ret, throws)) = found.map(|m| (m.ret, m.throws)) else {
        let n = env.solver.t.never();
        return (n, n);
    };
    let subject = subject_var(env, protocol);
    let Some(subject_name) = super::generic::as_var_name(&env.solver.t, subject) else {
        return (ret, throws);
    };
    let here = env.solver.t.name(param);
    let bound = env.solver.t.var(here);
    let subst = HashMap::from([(subject_name, bound)]);
    let ret = env.solver.t.substitute(ret, &subst);
    let throws = env.solver.t.substitute(throws, &subst);
    (ret, throws)
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
    let m = env.protocols()[protocol.0]
        .methods
        .iter()
        .find(|m| m.name == method)?;
    m.params.iter().position(|(_, t)| *t == subject)
}

/// The receiver's rigid variable name, if the receiver is exactly one bare variable.
///
/// Every other kind has to be checked empty, not just the primitive bases. `T | :none`
/// used to answer `Some("T")` — the atom arm was invisible here — so the call resolved to
/// `Resolution::Bound`, lowering could not name an impl for an abstract receiver, and the
/// program *ran* and printed `<todo: bound: abstract receiver>`. The same signature
/// written `T | null` set a base bit, fell through to `applicable`, and was a diagnostic:
/// a wrong answer or an error depending only on which kind the other arm lived in.
///
/// This is the same test `ordered::ordered_rec` makes on a variable, and for the same
/// reason: a variable mixed with anything else is a union, and a union is not rigid.
fn rigid_name(env: &Env, ty: TyId) -> Option<String> {
    let d = env.solver.t.data(ty);
    let vars = env.solver.t.atomset_of(d.vars);
    if vars.neg || vars.names.len() != 1 {
        return None;
    }
    // A union of a variable and anything at all is not a rigid receiver.
    if d.base != 0
        || !env.solver.t.atomset_of(d.atoms).is_empty_set()
        || d.records != super::bdd::FALSE
        || d.tuples != super::bdd::FALSE
        || d.arrows != super::bdd::FALSE
    {
        return None;
    }
    Some(env.solver.t.name_str(vars.names[0]).to_string())
}
