//! `ast::TypeSpec` -> `TyId`.
//!
//! Name lookup, generic instantiation and the μ-contractivity check live behind
//! `Env`; this is the syntax-directed half.

use super::env::{Env, TypeErrorKind};
use super::types::TyId;
use crate::ast::{TypeSpec, TypeSpecKind};

/// A type variable in scope: `T` in `fn f[T]`, or a protocol's subject.
///
/// `arity` is non-zero only for a constructor variable — the `C` of
/// `protocol Container for C[_]`, which is applied rather than used bare.
#[derive(Clone, Debug)]
pub struct ScopeVar {
    pub name: String,
    pub ty: TyId,
    pub arity: usize,
}

/// Where a `TypeSpec` is read. `module` decides name lookup and, because
/// `opaque` is module-scoped, which record fields are visible.
#[derive(Clone, Debug, Default)]
pub struct Scope {
    pub module: Vec<String>,
    pub vars: Vec<ScopeVar>,
}

impl Scope {
    pub fn new(module: &[String]) -> Self {
        Scope { module: module.to_vec(), vars: vec![] }
    }

    /// Bind `names` as rigid variables — the once-with-`T`-opaque reading of a
    /// generic signature.
    pub fn with_rigid(mut self, env: &mut Env, names: &[String]) -> Self {
        for n in names {
            let id = env.solver.t.name(n);
            let ty = env.solver.t.var(id);
            self.vars.push(ScopeVar { name: n.clone(), ty, arity: 0 });
        }
        self
    }

    fn find(&self, name: &str) -> Option<&ScopeVar> {
        self.vars.iter().rev().find(|v| v.name == name)
    }
}

/// Whether a struct field's annotation opts into null *by writing it* -- `T | null`
/// or a bare `null`. This reads the syntax, not the resolved type: `!i64` also admits
/// null but is a required field, and only the AST can still tell the two apart.
fn spec_admits_null(spec: &TypeSpec) -> bool {
    match &spec.kind {
        TypeSpecKind::Null => true,
        TypeSpecKind::Union(xs) => xs.iter().any(spec_admits_null),
        _ => false,
    }
}

pub fn resolve(env: &mut Env, scope: &Scope, spec: &TypeSpec) -> TyId {
    match &spec.kind {
        TypeSpecKind::Any => env.solver.t.any(),
        TypeSpecKind::Null => env.solver.t.null(),
        TypeSpecKind::Error => env.error_ty(),
        TypeSpecKind::Atom(a) => {
            let n = env.solver.t.name(a);
            env.solver.t.atom(n)
        }
        TypeSpecKind::Union(xs) => {
            let ts = resolve_all(env, scope, xs);
            or_poison(env, &ts, |e| e.solver.t.union_all(&ts))
        }
        TypeSpecKind::Intersect(xs) => {
            let ts = resolve_all(env, scope, xs);
            or_poison(env, &ts, |e| e.solver.t.intersect_all(&ts))
        }
        TypeSpecKind::Negate(x) => {
            let t = resolve(env, scope, x);
            or_poison(env, &[t], |e| {
                // `negate` complements within the field lattice, which includes the
                // "absent" marker. A user-written `!T` is a set of *values*, so the
                // marker has to come back off.
                let n = e.solver.t.negate(t);
                let any = e.solver.t.any();
                e.solver.t.intersect(n, any)
            })
        }
        TypeSpecKind::Tuple(xs) => {
            let ts = resolve_all(env, scope, xs);
            or_poison(env, &ts, |e| e.solver.t.tuple(ts.clone()))
        }
        TypeSpecKind::Fn { params, throws, ret } => {
            let ps = resolve_all(env, scope, params);
            // No `throws` clause is `never` — a function that throws nothing. `any`
            // would make every arrow a supertype of every other.
            let thr = match throws {
                Some(t) => resolve(env, scope, t),
                None => env.solver.t.never(),
            };
            let r = resolve(env, scope, ret);
            let mut ts = ps.clone();
            ts.push(thr);
            ts.push(r);
            or_poison(env, &ts, |e| e.solver.t.arrow(ps, thr, r))
        }
        TypeSpecKind::Struct(fields) => {
            let mut ts = Vec::with_capacity(fields.len());
            let mut labels: Vec<(super::types::NameId, TyId)> = Vec::new();
            for f in fields {
                let t = resolve(env, scope, &f.ty);
                ts.push(t);
                // A field written with an explicit `| null` is optional: a record that
                // omits it satisfies the type, the omission reading as null. The intent
                // is only visible here in the syntax -- once resolved, `i64 | null` and
                // `!i64` both admit null, but only the former opted in -- so the "may be
                // absent" marker is added now, from the shape of the annotation.
                let field = if spec_admits_null(&f.ty) {
                    let u = env.solver.t.undef();
                    env.solver.t.union(t, u)
                } else {
                    t
                };
                let l = env.solver.t.name(&f.name);
                if labels.iter().any(|(seen, _)| *seen == l) {
                    env.error(f.span.clone(), TypeErrorKind::DuplicateField(f.name.clone()));
                    return env.error_ty();
                }
                labels.push((l, field));
            }
            or_poison(env, &ts, |e| e.solver.t.struct_ty(labels.clone()))
        }
        TypeSpecKind::Named { path, args } => named(env, scope, spec, path, args),
    }
}

fn resolve_all(env: &mut Env, scope: &Scope, specs: &[TypeSpec]) -> Vec<TyId> {
    specs.iter().map(|s| resolve(env, scope, s)).collect()
}

/// Poison propagates to the top of a spec, so `is_error` is an id comparison and
/// one bad name costs one diagnostic rather than one per enclosing constructor.
fn or_poison(env: &mut Env, parts: &[TyId], f: impl FnOnce(&mut Env) -> TyId) -> TyId {
    if parts.contains(&env.error_ty()) {
        return env.error_ty();
    }
    f(env)
}

fn named(
    env: &mut Env,
    scope: &Scope,
    spec: &TypeSpec,
    path: &[String],
    args: &[TypeSpec],
) -> TyId {
    // Always resolved, even when the head is unknown or misapplied: the arguments
    // carry their own diagnostics, and an alias cycle running through one is only
    // visible if it is followed.
    let ts = resolve_all(env, scope, args);

    if let [only] = path {
        if let Some(v) = scope.find(only) {
            let (ty, arity) = (v.ty, v.arity);
            if arity != args.len() {
                env.error(
                    spec.span.clone(),
                    TypeErrorKind::Arity { name: only.clone(), expected: arity, found: args.len() },
                );
                return env.error_ty();
            }
            return or_poison(env, &ts, |e| {
                if arity == 0 {
                    ty
                } else {
                    // `C[T]` with `C` rigid: opaque, and covariant in its arguments,
                    // which is what a nominal already is.
                    let n = e.solver.t.name(only);
                    e.solver.t.nominal(n, ts.clone(), vec![])
                }
            });
        }
        if let Some(b) = primitive(env, only) {
            if !args.is_empty() {
                env.error(
                    spec.span.clone(),
                    TypeErrorKind::Arity { name: only.clone(), expected: 0, found: args.len() },
                );
                return env.error_ty();
            }
            return b;
        }
    }

    let Some(key) = env.lookup(&scope.module, path) else {
        env.error(spec.span.clone(), TypeErrorKind::Unknown(path.join("::")));
        return env.error_ty();
    };
    or_poison(env, &ts, |e| e.instantiate(&key, ts.clone(), &spec.span))
}

fn primitive(env: &mut Env, name: &str) -> Option<TyId> {
    match name {
        "i64" => Some(env.solver.t.i64()),
        "f64" => Some(env.solver.t.f64()),
        "str" => Some(env.solver.t.str()),
        "bool" => Some(env.solver.t.bool()),
        _ => None,
    }
}
