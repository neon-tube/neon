//! Lowering: the typed AST plus `TypecheckResult` become SSA. Nothing here re-derives
//! a type or re-resolves a call — every expression's type is read from `expr_types` and
//! turned into a `Repr`, and every dispatched call's decision is read from its
//! `Resolution`. See `docs/design/ir.md`.
//!
//! This is a growing pass: the scalar, control-flow, call and binding core is here, and
//! the richer forms (match, aggregates, closures, try) are layered on as the IR grows
//! to cover the corpus.

use super::repr::{repr_of, Repr};
use super::ssa::{Builder, BlockId, Func, Op, PrimOp, Program, Target, Term, Value};
use crate::ast::{self, BinOp, Block, Decl, DeclKind, Expr, ExprKind, Stmt, StmtKind, UnOp};
use crate::typecheck::env::Env;
use crate::typecheck::result::TypecheckResult;
use crate::typecheck::types::TyId;

/// A lambda whose body is lowered as its own function, discovered while lowering an
/// enclosing one. The captures are the free variables it closes over, in order.
struct LambdaJob {
    name: String,
    lambda: Expr,
    captures: Vec<(String, Repr, TyId)>,
    module: Vec<String>,
}

/// A concrete instance of a generic function, discovered at a call site. Monomorphisation
/// specialises the generic body under `subst`, mapping each generic parameter name to the
/// concrete `Repr` it was called with.
#[derive(Clone)]
struct InstanceJob {
    mangled: String,
    module: Vec<String>,
    fn_name: String,
    subst: std::collections::HashMap<String, Repr>,
}

/// Replace every type variable in a repr with its concrete binding. After this a
/// monomorphic instance has no `Var` left.
fn substitute_repr(r: &Repr, subst: &std::collections::HashMap<String, Repr>) -> Repr {
    match r {
        Repr::Var(n) => subst.get(n).cloned().unwrap_or_else(|| r.clone()),
        Repr::Record { name, fields } => Repr::Record {
            name: name.clone(),
            fields: fields.iter().map(|(n, r)| (n.clone(), substitute_repr(r, subst))).collect(),
        },
        Repr::Tuple(rs) => Repr::Tuple(rs.iter().map(|r| substitute_repr(r, subst)).collect()),
        Repr::Union(rs) => Repr::Union(rs.iter().map(|r| substitute_repr(r, subst)).collect()),
        Repr::List(e) => Repr::List(Box::new(substitute_repr(e, subst))),
        Repr::Nullable(e) => Repr::Nullable(Box::new(substitute_repr(e, subst))),
        Repr::Map(k, v) => {
            Repr::Map(Box::new(substitute_repr(k, subst)), Box::new(substitute_repr(v, subst)))
        }
        Repr::Closure { params, ret } => Repr::Closure {
            params: params.iter().map(|r| substitute_repr(r, subst)).collect(),
            ret: Box::new(substitute_repr(ret, subst)),
        },
        _ => r.clone(),
    }
}

/// Bind the type variables in a template repr to make it match a concrete one, for
/// building a call's instance substitution from its argument reprs.
fn match_repr(template: &Repr, concrete: &Repr, subst: &mut std::collections::HashMap<String, Repr>) {
    match (template, concrete) {
        (Repr::Var(n), c) => {
            subst.entry(n.clone()).or_insert_with(|| c.clone());
        }
        (Repr::List(a), Repr::List(b)) | (Repr::Nullable(a), Repr::Nullable(b)) => {
            match_repr(a, b, subst)
        }
        (Repr::Map(ak, av), Repr::Map(bk, bv)) => {
            match_repr(ak, bk, subst);
            match_repr(av, bv, subst);
        }
        (Repr::Record { fields: a, .. }, Repr::Record { fields: b, .. }) => {
            for ((_, at), (_, bt)) in a.iter().zip(b) {
                match_repr(at, bt, subst);
            }
        }
        (Repr::Tuple(a), Repr::Tuple(b)) | (Repr::Union(a), Repr::Union(b)) => {
            a.iter().zip(b).for_each(|(x, y)| match_repr(x, y, subst));
        }
        (Repr::Closure { params: ap, ret: ar }, Repr::Closure { params: bp, ret: br }) => {
            ap.iter().zip(bp).for_each(|(x, y)| match_repr(x, y, subst));
            match_repr(ar, br, subst);
        }
        _ => {}
    }
}

/// The mangled name of a generic instance: the base name with its concrete arguments.
fn mangle_instance(base: &str, subst: &std::collections::HashMap<String, Repr>) -> String {
    let mut keys: Vec<&String> = subst.keys().collect();
    keys.sort();
    let args: Vec<String> = keys.iter().map(|k| repr_key(&subst[*k])).collect();
    format!("{base}${}", args.join("$"))
}

/// A short, stable spelling of a repr for a mangled name.
fn repr_key(r: &Repr) -> String {
    match r {
        Repr::I64 => "i64".into(),
        Repr::F64 => "f64".into(),
        Repr::Bool => "bool".into(),
        Repr::Str => "str".into(),
        Repr::Null => "null".into(),
        Repr::Unit => "unit".into(),
        Repr::Tag => "tag".into(),
        Repr::Record { name: Some(n), .. } => n.clone(),
        Repr::Record { .. } => "rec".into(),
        Repr::List(e) => format!("list_{}", repr_key(e)),
        Repr::Map(k, v) => format!("map_{}_{}", repr_key(k), repr_key(v)),
        Repr::Tuple(rs) => format!("tup_{}", rs.iter().map(repr_key).collect::<Vec<_>>().join("_")),
        Repr::Nullable(e) => format!("opt_{}", repr_key(e)),
        Repr::Closure { .. } => "fn".into(),
        Repr::Union(_) => "union".into(),
        Repr::Var(n) => n.clone(),
        Repr::Recursive(_) => "rec".into(),
        Repr::Any => "any".into(),
        Repr::Never => "never".into(),
    }
}

/// Lower a whole module to a program of SSA functions. Lambdas are lowered as separate
/// functions via a worklist: lowering a function may discover lambdas, which are queued
/// and lowered in turn (and may discover more).
pub fn lower_module<'a>(env: &Env, result: &TypecheckResult, module: &'a ast::Module) -> Program {
    let mut funcs = Vec::new();
    let mut lambda_jobs: Vec<LambdaJob> = Vec::new();
    let mut instance_jobs: Vec<InstanceJob> = Vec::new();
    let mut lowered: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Every function body, keyed by (module, name), so a generic instance can find its
    // source when its call site discovers it.
    let mut all_fns: std::collections::HashMap<(Vec<String>, String), ast::FnDecl> =
        std::collections::HashMap::new();
    collect_all_fns(&[], &module.decls, &mut all_fns);

    // Non-generic top-level functions. A generic one is lowered only per instance, as its
    // call sites discover them.
    let mut fn_jobs: Vec<(Vec<String>, &'a ast::FnDecl)> = Vec::new();
    collect_fn_jobs(&[], &module.decls, &mut fn_jobs);
    for (m, f) in fn_jobs {
        if !f.generics.is_empty() {
            continue;
        }
        let (func, l, i) = lower_fn(env, result, &m, f);
        lowered.insert(func.name.clone());
        funcs.push(func);
        lambda_jobs.extend(l);
        instance_jobs.extend(i);
    }

    // Impl methods: correlate each `ImplDef`'s method (which carries the types) with its
    // AST body (which carries the code) through the same mangled name that dispatch uses.
    let mut impl_bodies: std::collections::HashMap<String, &ast::FnDecl> =
        std::collections::HashMap::new();
    collect_impl_bodies(&module.decls, &mut impl_bodies);
    for impl_def in env.impls() {
        let proto = env.protocols()[impl_def.protocol.0].name.clone();
        let head = impl_head(env, impl_def);
        for m in &impl_def.methods {
            if !m.has_body || !m.generics.is_empty() {
                continue;
            }
            let name = mangle_impl(&proto, &head, &m.name);
            if let Some(fd) = impl_bodies.get(&name) {
                let (func, l, i) = lower_method(env, result, &impl_def.module, fd, m, name);
                lowered.insert(func.name.clone());
                funcs.push(func);
                lambda_jobs.extend(l);
                instance_jobs.extend(i);
            }
        }
    }

    // Drain the worklists: lambdas and generic instances, deduplicated by name.
    loop {
        if let Some(job) = lambda_jobs.pop() {
            if !lowered.insert(job.name.clone()) {
                continue;
            }
            let (func, l, i) = lower_lambda_job(env, result, job);
            funcs.push(func);
            lambda_jobs.extend(l);
            instance_jobs.extend(i);
            continue;
        }
        if let Some(job) = instance_jobs.pop() {
            if !lowered.insert(job.mangled.clone()) {
                continue;
            }
            if let Some((func, l, i)) = lower_instance(env, result, &all_fns, job) {
                funcs.push(func);
                lambda_jobs.extend(l);
                instance_jobs.extend(i);
            }
            continue;
        }
        break;
    }
    Program { funcs }
}

fn collect_all_fns(
    module: &[String],
    decls: &[Decl],
    out: &mut std::collections::HashMap<(Vec<String>, String), ast::FnDecl>,
) {
    for d in decls {
        match &d.kind {
            DeclKind::Fn(f) if f.body.is_some() => {
                out.insert((module.to_vec(), f.name.clone()), f.clone());
            }
            DeclKind::Mod(m) => {
                let mut inner = module.to_vec();
                inner.push(m.name.clone());
                collect_all_fns(&inner, &m.decls, out);
            }
            _ => {}
        }
    }
}

/// Lower one concrete instance of a generic function under its substitution.
fn lower_instance(
    env: &Env,
    result: &TypecheckResult,
    all_fns: &std::collections::HashMap<(Vec<String>, String), ast::FnDecl>,
    job: InstanceJob,
) -> Option<(Func, Vec<LambdaJob>, Vec<InstanceJob>)> {
    let f = all_fns.get(&(job.module.clone(), job.fn_name.clone()))?;
    let sig = env.fn_named(&job.module, std::slice::from_ref(&job.fn_name));
    let ret_ty = sig.map(|s| s.ret).unwrap_or(TyId(0));
    let ret_repr = substitute_repr(&repr_of(&env.solver.t, ret_ty), &job.subst);

    let mut lo = Lower::with_subst(
        env,
        result,
        job.module.clone(),
        job.mangled.clone(),
        ret_repr.clone(),
        job.subst.clone(),
    );
    let mut params = Vec::new();
    for (i, p) in f.params.iter().enumerate() {
        let ty = sig.and_then(|s| s.params.get(i)).map(|(_, t)| *t).unwrap_or(TyId(0));
        let r = lo.repr_of_ty(ty);
        let v = lo.b.block_param(BlockId(0), r, ty);
        lo.bind(&p.name, v);
        params.push(v);
    }
    lo.b.switch_to(BlockId(0));
    let body = f.body.as_ref().expect("generic fn has a body");
    let tail = lo.lower_block(body);
    if !lo.terminated {
        let ret = if matches!(ret_repr, Repr::Unit) { None } else { tail };
        lo.b.terminate(Term::Ret(ret));
    }
    let (l, i) = (std::mem::take(&mut lo.pending), std::mem::take(&mut lo.instances));
    Some((lo.b.finish(params), l, i))
}

fn collect_fn_jobs<'a>(
    module: &[String],
    decls: &'a [Decl],
    out: &mut Vec<(Vec<String>, &'a ast::FnDecl)>,
) {
    for d in decls {
        match &d.kind {
            DeclKind::Fn(f) if f.body.is_some() => out.push((module.to_vec(), f)),
            DeclKind::Mod(m) => {
                let mut inner = module.to_vec();
                inner.push(m.name.clone());
                collect_fn_jobs(&inner, &m.decls, out);
            }
            _ => {}
        }
    }
}

/// The mangled name of a function. Monomorphisation will refine this with concrete type
/// arguments; for now it is the module path and name.
fn mangle(module: &[String], name: &str) -> String {
    if module.is_empty() {
        name.to_string()
    } else {
        format!("{}__{name}", module.join("__"))
    }
}

/// The name of an impl method, agreed between the site that dispatches to it and the
/// site that lowers it: protocol, target head, and method.
fn mangle_impl(protocol: &str, head: &str, method: &str) -> String {
    format!("{protocol}${head}${method}")
}

/// The head of an impl's target, for mangling — the nominal or primitive name.
fn impl_head(env: &Env, impl_def: &crate::typecheck::env::ImplDef) -> String {
    if let Some(h) = &impl_def.target_head {
        return h.clone();
    }
    match impl_def.target.map(|t| repr_of(&env.solver.t, t)) {
        Some(Repr::Record { name: Some(n), .. }) => n,
        Some(Repr::I64) => "i64".into(),
        Some(Repr::F64) => "f64".into(),
        Some(Repr::Str) => "str".into(),
        Some(Repr::Bool) => "bool".into(),
        _ => String::new(),
    }
}

/// The head name of a repr, for matching a bound-dispatch receiver to an impl.
fn repr_head(r: &Repr) -> Option<String> {
    Some(match r {
        Repr::Record { name: Some(n), .. } => n.clone(),
        Repr::I64 => "i64".into(),
        Repr::F64 => "f64".into(),
        Repr::Str => "str".into(),
        Repr::Bool => "bool".into(),
        Repr::List(_) => "List".into(),
        Repr::Map(_, _) => "Map".into(),
        _ => return None,
    })
}

/// Find the impl of `protocol` for a type whose head is `head`, and its method — its
/// native symbol (if any) and what it throws. This discharges a `where` bound once the
/// receiver is concrete.
fn find_impl_method(
    env: &Env,
    protocol: crate::typecheck::env::ProtocolId,
    head: &str,
    method: &str,
) -> Option<(Option<String>, TyId)> {
    for impl_def in env.impls() {
        if impl_def.protocol == protocol && impl_head(env, impl_def) == head {
            if let Some(m) = impl_def.methods.iter().find(|m| m.name == method) {
                return Some((m.native.clone(), m.throws));
            }
        }
    }
    None
}

/// The head of an impl target written in the AST, matching `impl_head`.
fn ast_head(ty: &ast::TypeSpec) -> String {
    match &ty.kind {
        ast::TypeSpecKind::Named { path, .. } => path.last().cloned().unwrap_or_default(),
        _ => String::new(),
    }
}

fn collect_impl_bodies<'a>(
    decls: &'a [Decl],
    out: &mut std::collections::HashMap<String, &'a ast::FnDecl>,
) {
    for d in decls {
        match &d.kind {
            DeclKind::Impl(i) => {
                let proto = i.protocol.last().cloned().unwrap_or_default();
                let head = ast_head(&i.target);
                for m in &i.methods {
                    if m.body.is_some() {
                        out.insert(mangle_impl(&proto, &head, &m.name), m);
                    }
                }
            }
            DeclKind::Mod(m) => collect_impl_bodies(&m.decls, out),
            _ => {}
        }
    }
}

/// Lower an impl method, whose types come from its `FnSig` and whose code from the AST.
fn lower_method(
    env: &Env,
    result: &TypecheckResult,
    module: &[String],
    f: &ast::FnDecl,
    sig: &crate::typecheck::env::FnSig,
    name: String,
) -> (Func, Vec<LambdaJob>, Vec<InstanceJob>) {
    let ret_repr = repr_of(&env.solver.t, sig.ret);
    let mut lo = Lower::new(env, result, module.to_vec(), name, ret_repr.clone());
    let mut params = Vec::new();
    for (i, p) in f.params.iter().enumerate() {
        let ty = sig.params.get(i).map(|(_, t)| *t).unwrap_or(TyId(0));
        let r = repr_of(&env.solver.t, ty);
        let v = lo.b.block_param(BlockId(0), r, ty);
        lo.bind(&p.name, v);
        params.push(v);
    }
    lo.b.switch_to(BlockId(0));
    let body = f.body.as_ref().expect("filtered to bodied methods");
    let tail = lo.lower_block(body);
    if !lo.terminated {
        let ret = if matches!(ret_repr, Repr::Unit) { None } else { tail };
        lo.b.terminate(Term::Ret(ret));
    }
    let (l, i) = (std::mem::take(&mut lo.pending), std::mem::take(&mut lo.instances));
    (lo.b.finish(params), l, i)
}

fn lower_fn(
    env: &Env,
    result: &TypecheckResult,
    module: &[String],
    f: &ast::FnDecl,
) -> (Func, Vec<LambdaJob>, Vec<InstanceJob>) {
    let name = f.name.clone();
    let sig = env.fn_named(module, std::slice::from_ref(&name));
    let ret_ty = sig.map(|s| s.ret).unwrap_or(TyId(0));
    let ret_repr = repr_of(&env.solver.t, ret_ty);

    let mut lo = Lower::new(env, result, module.to_vec(), mangle(module, &f.name), ret_repr.clone());

    // Parameters are the entry block's parameters.
    let mut params = Vec::new();
    for (i, p) in f.params.iter().enumerate() {
        let ty = sig.and_then(|s| s.params.get(i)).map(|(_, t)| *t).unwrap_or(TyId(0));
        let r = repr_of(&env.solver.t, ty);
        let v = lo.b.block_param(BlockId(0), r, ty);
        lo.bind(&p.name, v);
        params.push(v);
    }
    lo.b.switch_to(BlockId(0));

    let body = f.body.as_ref().expect("filtered to bodied fns");
    let tail = lo.lower_block(body);
    if !lo.terminated {
        let ret = if matches!(ret_repr, Repr::Unit) { None } else { tail };
        lo.b.terminate(Term::Ret(ret));
    }
    let (l, i) = (std::mem::take(&mut lo.pending), std::mem::take(&mut lo.instances));
    (lo.b.finish(params), l, i)
}

/// Lower a lambda's body as its own function. Its first parameter is the environment (a
/// tuple of the captured values); the rest are the lambda's parameters.
fn lower_lambda_job(env: &Env, result: &TypecheckResult, job: LambdaJob) -> (Func, Vec<LambdaJob>, Vec<InstanceJob>) {
    let ExprKind::Lambda { params: lparams, body } = &job.lambda.kind else {
        unreachable!("a lambda job holds a lambda");
    };
    // The lambda's inferred arrow gives its parameter and return reprs.
    let (param_reprs, ret_repr) = match result.ty(job.lambda.id).map(|t| repr_of(&env.solver.t, t)) {
        Some(Repr::Closure { params, ret }) => (params, *ret),
        _ => (vec![], Repr::Unit),
    };

    let mut lo = Lower::new(env, result, job.module.clone(), job.name.clone(), ret_repr.clone());

    // The environment parameter, then unpack each capture from it.
    let env_repr = Repr::Tuple(job.captures.iter().map(|(_, r, _)| r.clone()).collect());
    let env_v = lo.b.block_param(BlockId(0), env_repr, TyId(0));
    let mut params = vec![env_v];
    if !job.captures.is_empty() {
        for (i, (n, r, cty)) in job.captures.iter().enumerate() {
            let cap = lo.b.emit(Op::Elem { base: env_v, index: i }, r.clone(), *cty);
            lo.bind(n, cap);
        }
    }
    for (i, p) in lparams.iter().enumerate() {
        let r = param_reprs.get(i).cloned().unwrap_or(Repr::Any);
        let v = lo.b.block_param(BlockId(0), r, TyId(0));
        lo.bind(&p.name, v);
        params.push(v);
    }
    lo.b.switch_to(BlockId(0));

    let tail = lo.lower_expr(body);
    if !lo.terminated {
        let ret = if matches!(ret_repr, Repr::Unit) { None } else { Some(tail) };
        lo.b.terminate(Term::Ret(ret));
    }
    let (l, i) = (std::mem::take(&mut lo.pending), std::mem::take(&mut lo.instances));
    (lo.b.finish(params), l, i)
}

struct Lower<'a> {
    env: &'a Env,
    result: &'a TypecheckResult,
    /// The module the current function is in, for resolving call targets.
    module: Vec<String>,
    b: Builder,
    /// Local bindings, innermost last: a name resolves to its SSA value.
    scope: Vec<Vec<(String, Value)>>,
    /// Whether the current block already has a terminator (a `return` was lowered), so
    /// the statements after it are dead and must not be emitted.
    terminated: bool,
    /// The enclosing loops, innermost last, for `break` and `continue`.
    loops: Vec<LoopCtx>,
    /// The enclosing `try` handlers, innermost last: the block a throwing call or a
    /// `throw` jumps to on error, passing the error value. Empty means an error
    /// propagates straight out of the function.
    handlers: Vec<BlockId>,
    /// Lambdas discovered while lowering this function, to be lowered as their own.
    pending: Vec<LambdaJob>,
    /// This instance's type-variable bindings (empty for a non-generic function).
    subst: std::collections::HashMap<String, Repr>,
    /// Generic instances discovered at call sites, to be lowered in turn.
    instances: Vec<InstanceJob>,
}

impl<'a> Lower<'a> {
    fn new(
        env: &'a Env,
        result: &'a TypecheckResult,
        module: Vec<String>,
        fn_name: String,
        ret: Repr,
    ) -> Self {
        Self::with_subst(env, result, module, fn_name, ret, Default::default())
    }

    fn with_subst(
        env: &'a Env,
        result: &'a TypecheckResult,
        module: Vec<String>,
        fn_name: String,
        ret: Repr,
        subst: std::collections::HashMap<String, Repr>,
    ) -> Self {
        Lower {
            env,
            result,
            module,
            b: Builder::new(fn_name, ret),
            scope: vec![vec![]],
            terminated: false,
            loops: vec![],
            handlers: vec![],
            pending: vec![],
            subst,
            instances: vec![],
        }
    }
}

/// What `break` and `continue` need: where each jumps, and the loop-carried variables to
/// pass at the back-edge. `continue_target` is the header for `loop`/`while` and the
/// latch (which increments the index) for `for`; both take the carried variables.
struct LoopCtx {
    continue_target: BlockId,
    exit: BlockId,
    carried: Vec<String>,
    /// Whether the loop yields a value (`break e`), so the exit block takes an argument.
    has_value: bool,
}

impl Lower<'_> {
    fn bind(&mut self, name: &str, v: Value) {
        self.scope.last_mut().unwrap().push((name.to_string(), v));
    }

    fn lookup(&self, name: &str) -> Option<Value> {
        self.scope.iter().rev().flat_map(|s| s.iter().rev()).find(|(n, _)| n == name).map(|(_, v)| *v)
    }

    fn repr(&self, e: &Expr) -> Repr {
        match self.result.ty(e.id) {
            Some(ty) => self.repr_of_ty(ty),
            None => Repr::Unit,
        }
    }

    /// The repr of a type, with this instance's type variables substituted away.
    fn repr_of_ty(&self, ty: TyId) -> Repr {
        let r = repr_of(&self.env.solver.t, ty);
        if self.subst.is_empty() {
            r
        } else {
            substitute_repr(&r, &self.subst)
        }
    }

    fn ty(&self, e: &Expr) -> TyId {
        self.result.ty(e.id).unwrap_or(TyId(0))
    }

    // ---- blocks and statements ----

    /// Lower a block, returning its value (the tail expression's), or `None` for a
    /// statement-sequence block.
    fn lower_block(&mut self, block: &Block) -> Option<Value> {
        self.scope.push(vec![]);
        for s in &block.stmts {
            if self.terminated {
                break;
            }
            self.lower_stmt(s);
        }
        let tail = match &block.tail {
            Some(e) if !self.terminated => Some(self.lower_expr(e)),
            _ => None,
        };
        self.scope.pop();
        tail
    }

    fn lower_stmt(&mut self, s: &Stmt) {
        match &s.kind {
            StmtKind::Let { pat, value, .. } => {
                let v = self.lower_expr(value);
                self.bind_pattern(pat, v);
            }
            StmtKind::Assign { name, value } => {
                // A rebind is a fresh SSA value shadowing the old binding.
                let v = self.lower_expr(value);
                self.bind(name, v);
            }
            StmtKind::Expr(e) => {
                self.lower_expr(e);
            }
            StmtKind::Error => {}
        }
    }

    /// Bind the names a pattern introduces to a value. Only the irrefutable shapes reach
    /// here (a `let`); refutable patterns live in `match`.
    fn bind_pattern(&mut self, p: &ast::Pattern, v: Value) {
        match &p.kind {
            ast::PatternKind::Bind(n) => self.bind(n, v),
            ast::PatternKind::Wildcard => {}
            ast::PatternKind::Tuple(ps) => {
                for (i, sub) in ps.iter().enumerate() {
                    let r = elem_repr(&self.b, v, i);
                    let ty = self.b.value_ty(v);
                    let e = self.b.emit(Op::Elem { base: v, index: i }, r, ty);
                    self.bind_pattern(sub, e);
                }
            }
            ast::PatternKind::Record { fields, .. } => {
                for f in fields {
                    let r = field_repr(&self.b, v, &f.name);
                    let ty = self.b.value_ty(v);
                    let e = self.b.emit(Op::Field { base: v, field: f.name.clone() }, r, ty);
                    match &f.pat {
                        Some(sub) => self.bind_pattern(sub, e),
                        None => self.bind(&f.name, e),
                    }
                }
            }
            _ => {}
        }
    }

    // ---- expressions ----

    fn lower_expr(&mut self, e: &Expr) -> Value {
        let repr = self.repr(e);
        let ty = self.ty(e);
        match &e.kind {
            ExprKind::Int(n) => self.b.emit(Op::ConstI64(*n as i64), repr, ty),
            ExprKind::Float(s) => {
                let bits = s.parse::<f64>().unwrap_or(0.0).to_bits();
                self.b.emit(Op::ConstF64(bits), repr, ty)
            }
            ExprKind::Bool(b) => self.b.emit(Op::ConstBool(*b), repr, ty),
            ExprKind::Null => self.b.emit(Op::ConstNull, repr, ty),
            ExprKind::Atom(a) => self.b.emit(Op::ConstAtom(a.clone()), repr, ty),
            ExprKind::Str(parts) => self.lower_str(parts, repr, ty),
            ExprKind::Path(p) => self.lower_path(p, repr, ty),
            ExprKind::Unary { op, rhs } => {
                let r = self.lower_expr(rhs);
                self.b.emit(Op::Prim(un_prim(*op), vec![r]), repr, ty)
            }
            ExprKind::Binary { op, lhs, rhs } => self.lower_binary(*op, lhs, rhs, repr, ty),
            ExprKind::Call { callee, args, .. } => self.lower_call(e.id, callee, args, repr, ty),
            ExprKind::If { cond, then, else_ } => self.lower_if(cond, then, else_.as_deref(), repr, ty),
            ExprKind::Match { scrutinee, arms } => self.lower_match(scrutinee, arms, repr, ty),
            ExprKind::Loop { body } => self.lower_loop(body, repr, ty),
            ExprKind::While { cond, body } => self.lower_while(cond, body, ty),
            ExprKind::For { pat, iter, body } => self.lower_for(pat, iter, body, ty),
            ExprKind::Break(v) => {
                let bv = match v {
                    Some(e) => self.lower_expr(e),
                    None => self.b.emit(Op::ConstNull, Repr::Null, ty),
                };
                if let Some(ctx) = self.loops.last() {
                    let (exit, has_value) = (ctx.exit, ctx.has_value);
                    let args = if has_value { vec![bv] } else { vec![] };
                    self.b.terminate(Term::Jump(Target { to: exit, args }));
                }
                self.terminated = true;
                self.b.value(Repr::Never, ty)
            }
            ExprKind::Continue => {
                self.jump_to_header();
                self.terminated = true;
                self.b.value(Repr::Never, ty)
            }
            ExprKind::Block(b) => self.lower_block(b).unwrap_or_else(|| self.unit(ty)),
            ExprKind::Field { base, name } => {
                let b = self.lower_expr(base);
                self.b.emit(Op::Field { base: b, field: name.clone() }, repr, ty)
            }
            ExprKind::Tuple(elems) => {
                let vs = elems.iter().map(|e| self.lower_expr(e)).collect();
                self.b.emit(Op::MakeTuple(vs), repr, ty)
            }
            ExprKind::List(elems) => self.lower_list(elems, repr, ty),
            ExprKind::RecordLit { path, fields, spread } => {
                self.lower_record(path.as_deref(), fields, spread.as_deref(), repr, ty)
            }
            ExprKind::Index { base, index } => {
                let b = self.lower_expr(base);
                let i = self.lower_expr(index);
                self.b.emit(Op::Index { base: b, index: i }, repr, ty)
            }
            ExprKind::Is { lhs, ty: spec } => {
                let v = self.lower_expr(lhs);
                self.type_test(v, spec)
            }
            ExprKind::As { lhs, .. } => {
                let v = self.lower_expr(lhs);
                self.b.emit(Op::Cast(v), repr, ty)
            }
            ExprKind::Try { form, body, catch } => {
                self.lower_try(*form, body, catch.as_ref(), repr, ty)
            }
            ExprKind::Lambda { .. } => self.lower_lambda(e, repr, ty),
            ExprKind::Throw(e) => {
                let ev = self.lower_expr(e);
                match self.handlers.last().copied() {
                    Some(h) => self.b.terminate(Term::Jump(Target { to: h, args: vec![ev] })),
                    None => self.b.terminate(Term::Throw(ev)),
                }
                self.terminated = true;
                self.b.value(Repr::Never, ty)
            }
            ExprKind::Return(v) => {
                let rv = v.as_ref().map(|e| self.lower_expr(e));
                self.b.terminate(Term::Ret(rv));
                self.terminated = true;
                // The value of a `return` is never consumed; mint one without emitting.
                self.b.value(Repr::Never, ty)
            }
            _ => self.unhandled(e, repr, ty),
        }
    }

    fn lower_list(&mut self, elems: &[ast::Elem], repr: Repr, ty: TyId) -> Value {
        // A spread (`..rest`) is a concatenation; not lowered yet, so mark it.
        if elems.iter().any(|e| matches!(e, ast::Elem::Spread(_))) {
            return self.unhandled_note("list spread", repr, ty);
        }
        let vs = elems
            .iter()
            .map(|e| match e {
                ast::Elem::Value(x) => self.lower_expr(x),
                ast::Elem::Spread(_) => unreachable!("guarded above"),
            })
            .collect();
        self.b.emit(Op::MakeList(vs), repr, ty)
    }

    fn lower_record(
        &mut self,
        path: Option<&[String]>,
        fields: &[ast::FieldInit],
        spread: Option<&Expr>,
        repr: Repr,
        ty: TyId,
    ) -> Value {
        if spread.is_some() {
            return self.unhandled_note("record spread", repr, ty);
        }
        // Emit fields in the repr's canonical order, so every value of a type is built
        // the same way. A field the literal omits is a nullable optional -> null.
        let order: Vec<String> = match &repr {
            Repr::Record { fields, .. } => fields.iter().map(|(n, _)| n.clone()).collect(),
            _ => fields.iter().map(|f| f.name.clone()).collect(),
        };
        let name = path.and_then(|p| p.last().cloned());
        let mut built = Vec::new();
        for fname in order {
            let v = match fields.iter().find(|f| f.name == fname) {
                Some(f) => self.lower_expr(&f.value),
                None => self.b.emit(Op::ConstNull, Repr::Null, ty),
            };
            built.push((fname, v));
        }
        self.b.emit(Op::MakeRecord { name, fields: built }, repr, ty)
    }

    fn lower_str(&mut self, parts: &[ast::StrPart], repr: Repr, ty: TyId) -> Value {
        if let [ast::StrPart::Text(s)] = parts {
            return self.b.emit(Op::ConstStr(s.clone()), repr.clone(), ty);
        }
        if parts.is_empty() {
            return self.b.emit(Op::ConstStr(String::new()), repr, ty);
        }
        // Interpolation: each hole is `to_string`'d, each text chunk is a literal, and
        // the pieces are concatenated left to right.
        let mut acc: Option<Value> = None;
        for part in parts {
            let piece = match part {
                ast::StrPart::Text(s) => self.b.emit(Op::ConstStr(s.clone()), Repr::Str, ty),
                ast::StrPart::Interp(e) => {
                    let v = self.lower_expr(e);
                    let vr = self.b.value_repr(v).clone();
                    match to_string_symbol(&vr) {
                        Some(sym) => self.b.emit(Op::Native { symbol: sym, args: vec![v] }, Repr::Str, ty),
                        // A str hole is already a string.
                        None if matches!(vr, Repr::Str) => v,
                        // A nominal hole dispatches to its Display impl's `to_string`.
                        None => match &vr {
                            Repr::Record { name: Some(n), .. } => self.b.emit(
                                Op::Call { func: mangle_impl("Display", n, "to_string"), args: vec![v] },
                                Repr::Str,
                                ty,
                            ),
                            _ => return self.unhandled_note("string interpolation of a value type", repr, ty),
                        },
                    }
                }
            };
            acc = Some(match acc {
                None => piece,
                Some(a) => self.b.emit(
                    Op::Native { symbol: "neon_str_concat".into(), args: vec![a, piece] },
                    Repr::Str,
                    ty,
                ),
            });
        }
        acc.unwrap_or_else(|| self.b.emit(Op::ConstStr(String::new()), repr, ty))
    }

    fn lower_path(&mut self, p: &[String], repr: Repr, ty: TyId) -> Value {
        if let [name] = p {
            if let Some(v) = self.lookup(name) {
                return v;
            }
        }
        // A bare function name used as a value: a closure with no captured environment.
        if let Some(sig) = self.env.fn_named(&self.module, p) {
            let func = mangle(&sig.module, &sig.name);
            return self.b.emit(Op::MakeClosure { func, captures: vec![] }, repr, ty);
        }
        self.unhandled_note("path-as-value", repr, ty)
    }

    /// `(x) => e` — capture the free variables, queue the body to be lowered as its own
    /// function, and build a closure of the two.
    fn lower_lambda(&mut self, e: &Expr, repr: Repr, ty: TyId) -> Value {
        let captures = self.free_vars(e);
        let capture_vals: Vec<Value> = captures.iter().map(|n| self.lookup(n).unwrap()).collect();
        let cap_info: Vec<(String, Repr, TyId)> = captures
            .iter()
            .zip(&capture_vals)
            .map(|(n, &v)| (n.clone(), self.b.value_repr(v).clone(), self.b.value_ty(v)))
            .collect();
        let name = format!("lambda${}", e.id.0);
        self.pending.push(LambdaJob {
            name: name.clone(),
            lambda: e.clone(),
            captures: cap_info,
            module: self.module.clone(),
        });
        self.b.emit(Op::MakeClosure { func: name, captures: capture_vals }, repr, ty)
    }

    /// The free variables a lambda closes over: names its body uses that are bound in the
    /// enclosing scope, excluding its own parameters and locals.
    fn free_vars(&self, e: &Expr) -> Vec<String> {
        let ExprKind::Lambda { params, body } = &e.kind else { return vec![] };
        let mut bound: std::collections::HashSet<String> =
            params.iter().map(|p| p.name.clone()).collect();
        let mut used = Vec::new();
        collect_free_expr(body, &mut bound, &mut used);
        let mut seen = std::collections::HashSet::new();
        used.retain(|n| self.lookup(n).is_some() && seen.insert(n.clone()));
        used
    }

    fn lower_binary(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr, repr: Repr, ty: TyId) -> Value {
        if let Some(p) = bin_prim(op) {
            let l = self.lower_expr(lhs);
            let r = self.lower_expr(rhs);
            return self.b.emit(Op::Prim(p, vec![l, r]), repr, ty);
        }
        match op {
            BinOp::And | BinOp::Or => self.lower_and_or(op, lhs, rhs, ty),
            BinOp::Orelse => self.lower_orelse(lhs, rhs, repr, ty),
            BinOp::Pipe => self.lower_pipe(lhs, rhs, repr, ty),
            _ => self.unhandled_note("binary op", repr, ty),
        }
    }

    /// `and`/`or` short-circuit: the right operand is only evaluated when the left does
    /// not already decide the result.
    fn lower_and_or(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr, ty: TyId) -> Value {
        let l = self.lower_expr(lhs);
        let rhs_b = self.b.new_block();
        let short_b = self.b.new_block();
        let join = self.b.new_block();
        let jp = self.b.block_param(join, Repr::Bool, ty);

        // `and`: `l` false shorts to false, else evaluate `rhs`. `or`: `l` true shorts
        // to true, else evaluate `rhs`.
        let (then_tgt, else_tgt, short_const) = match op {
            BinOp::And => (rhs_b, short_b, false),
            BinOp::Or => (short_b, rhs_b, true),
            _ => unreachable!(),
        };
        self.b.terminate(Term::Branch {
            cond: l,
            then: Target { to: then_tgt, args: vec![] },
            els: Target { to: else_tgt, args: vec![] },
        });

        self.b.switch_to(rhs_b);
        self.terminated = false;
        let r = self.lower_expr(rhs);
        if !self.terminated {
            self.b.terminate(Term::Jump(Target { to: join, args: vec![r] }));
        }

        self.b.switch_to(short_b);
        self.terminated = false;
        let sv = self.b.emit(Op::ConstBool(short_const), Repr::Bool, ty);
        self.b.terminate(Term::Jump(Target { to: join, args: vec![sv] }));

        self.b.switch_to(join);
        self.terminated = false;
        jp
    }

    /// `a orelse b` — `b` when `a` is null, else `a`'s non-null value.
    fn lower_orelse(&mut self, lhs: &Expr, rhs: &Expr, repr: Repr, ty: TyId) -> Value {
        let l = self.lower_expr(lhs);
        let lty = self.b.value_ty(l);
        let isnull = self.b.emit(Op::IsNull(l), Repr::Bool, lty);
        let none_b = self.b.new_block();
        let some_b = self.b.new_block();
        let join = self.b.new_block();
        let jp = self.b.block_param(join, repr.clone(), ty);

        self.b.terminate(Term::Branch {
            cond: isnull,
            then: Target { to: none_b, args: vec![] },
            els: Target { to: some_b, args: vec![] },
        });

        self.b.switch_to(none_b);
        self.terminated = false;
        let r = self.lower_expr(rhs);
        if !self.terminated {
            self.b.terminate(Term::Jump(Target { to: join, args: vec![r] }));
        }

        self.b.switch_to(some_b);
        self.terminated = false;
        // The non-null value, reinterpreted to the non-null repr.
        let unwrapped = self.b.emit(Op::Cast(l), repr, ty);
        self.b.terminate(Term::Jump(Target { to: join, args: vec![unwrapped] }));

        self.b.switch_to(join);
        self.terminated = false;
        jp
    }

    /// `a |> f(b)` is `f(a, b)` — the pipe threads its left side as the first argument.
    fn lower_pipe(&mut self, lhs: &Expr, rhs: &Expr, repr: Repr, ty: TyId) -> Value {
        let ExprKind::Call { callee, args, .. } = &rhs.kind else {
            return self.unhandled_note("pipe rhs", repr, ty);
        };
        let mut arg_vs = vec![self.lower_expr(lhs)];
        arg_vs.extend(args.iter().map(|a| self.lower_expr(a)));
        self.lower_call_vals(rhs.id, callee, arg_vs, repr, ty)
    }

    fn lower_call(
        &mut self,
        id: crate::ast::ExprId,
        callee: &Expr,
        args: &[Expr],
        repr: Repr,
        ty: TyId,
    ) -> Value {
        let arg_vs: Vec<Value> = args.iter().map(|a| self.lower_expr(a)).collect();
        self.lower_call_vals(id, callee, arg_vs, repr, ty)
    }

    /// Lower a call whose arguments are already lowered (shared by `f(..)` and pipe).
    fn lower_call_vals(
        &mut self,
        id: crate::ast::ExprId,
        callee: &Expr,
        arg_vs: Vec<Value>,
        repr: Repr,
        ty: TyId,
    ) -> Value {
        // A dispatched call: the checker already chose the impl.
        if let Some(res) = self.result.call(id) {
            return self.lower_dispatch(res, callee, arg_vs, repr, ty);
        }

        // A call through a local of arrow type is a closure call.
        if let ExprKind::Path(p) = &callee.kind {
            if let [one] = p.as_slice() {
                if let Some(callee_v) = self.lookup(one) {
                    return self.b.emit(Op::CallClosure { callee: callee_v, args: arg_vs }, repr, ty);
                }
            }
            // A direct call to a named module function: native symbol or a Neon body.
            if let Some(sig) = self.env.fn_named(&self.module, p) {
                let throws = sig.throws;
                let native = sig.native.clone();
                let is_generic = !sig.generics.is_empty();
                let (smodule, sname) = (sig.module.clone(), sig.name.clone());
                let param_tys: Vec<TyId> = sig.params.iter().map(|(_, t)| *t).collect();
                let ret_ty = sig.ret;

                if is_generic && native.is_none() {
                    // Specialise: build the substitution from the argument reprs (and the
                    // return, for a type variable that only appears there), then call and
                    // queue the concrete instance.
                    let mut subst = std::collections::HashMap::new();
                    for (i, &av) in arg_vs.iter().enumerate() {
                        if let Some(&pty) = param_tys.get(i) {
                            let template = repr_of(&self.env.solver.t, pty);
                            let concrete = self.b.value_repr(av).clone();
                            match_repr(&template, &concrete, &mut subst);
                        }
                    }
                    let ret_template = repr_of(&self.env.solver.t, ret_ty);
                    match_repr(&ret_template, &repr, &mut subst);
                    let mangled = mangle_instance(&mangle(&smodule, &sname), &subst);
                    self.instances.push(InstanceJob {
                        mangled: mangled.clone(),
                        module: smodule,
                        fn_name: sname,
                        subst,
                    });
                    let result = self.b.emit(Op::Call { func: mangled, args: arg_vs }, repr.clone(), ty);
                    return self.wrap_throwing(result, throws, repr, ty);
                }

                let op = match native {
                    Some(sym) => Op::Native { symbol: sym, args: arg_vs },
                    None => Op::Call { func: mangle(&smodule, &sname), args: arg_vs },
                };
                let result = self.b.emit(op, repr.clone(), ty);
                return self.wrap_throwing(result, throws, repr, ty);
            }
        }
        self.unhandled_note("call target", repr, ty)
    }

    /// Lower a call the checker resolved by protocol dispatch. A `Direct` to a native
    /// impl (the primitives) becomes a native call; the rest — user impls, switches,
    /// and generic bounds — are lowered in a later pass and marked for now.
    fn lower_dispatch(
        &mut self,
        res: &crate::typecheck::dispatch::Resolution,
        callee: &Expr,
        args: Vec<Value>,
        repr: Repr,
        ty: TyId,
    ) -> Value {
        use crate::typecheck::dispatch::Resolution;
        let method = match &callee.kind {
            ExprKind::Path(p) => p.last().cloned().unwrap_or_default(),
            _ => return self.unhandled_note("dispatch callee", repr, ty),
        };
        match res {
            Resolution::Direct(impl_id) => {
                let impl_def = &self.env.impls()[impl_id.0];
                let m = impl_def.methods.iter().find(|m| m.name == method);
                let Some(m) = m else {
                    return self.unhandled_note("dispatch: no method", repr, ty);
                };
                let throws = m.throws;
                let op = match &m.native {
                    Some(sym) => Op::Native { symbol: sym.clone(), args },
                    None => {
                        // A user impl: call the method's own lowered function.
                        let proto = self.env.protocols()[impl_def.protocol.0].name.clone();
                        let head = impl_head(self.env, impl_def);
                        Op::Call { func: mangle_impl(&proto, &head, &method), args }
                    }
                };
                let result = self.b.emit(op, repr.clone(), ty);
                self.wrap_throwing(result, throws, repr, ty)
            }
            Resolution::Switch(_) => self.unhandled_note("dispatch switch", repr, ty),
            Resolution::Bound { protocol, .. } => {
                // In a monomorphic instance the receiver is concrete, so its head picks
                // the impl the bound stood for.
                let recv = args.first().copied();
                let head = recv.and_then(|v| repr_head(self.b.value_repr(v)));
                let proto = self.env.protocols()[protocol.0].name.clone();
                match head {
                    Some(h) => {
                        let found = find_impl_method(self.env, *protocol, &h, &method);
                        match found {
                            Some((Some(sym), throws)) => {
                                let result = self.b.emit(Op::Native { symbol: sym, args }, repr.clone(), ty);
                                self.wrap_throwing(result, throws, repr, ty)
                            }
                            Some((None, throws)) => {
                                let result = self.b.emit(
                                    Op::Call { func: mangle_impl(&proto, &h, &method), args },
                                    repr.clone(),
                                    ty,
                                );
                                self.wrap_throwing(result, throws, repr, ty)
                            }
                            None => self.unhandled_note("bound: no impl", repr, ty),
                        }
                    }
                    None => self.unhandled_note("bound: abstract receiver", repr, ty),
                }
            }
        }
    }

    fn lower_if(
        &mut self,
        cond: &Expr,
        then: &Block,
        else_: Option<&Expr>,
        repr: Repr,
        ty: TyId,
    ) -> Value {
        let cond_v = self.lower_expr(cond);
        let then_b = self.b.new_block();
        let else_b = self.b.new_block();
        let join = self.b.new_block();
        let produces = !matches!(repr, Repr::Unit) && else_.is_some();
        let join_param = produces.then(|| self.b.block_param(join, repr.clone(), ty));

        self.b.terminate(Term::Branch {
            cond: cond_v,
            then: Target { to: then_b, args: vec![] },
            els: Target { to: else_b, args: vec![] },
        });

        // then
        self.b.switch_to(then_b);
        self.terminated = false;
        let tv = self.lower_block(then);
        if !self.terminated {
            let args = join_param.map(|_| vec![tv.unwrap_or_else(|| self.unit(ty))]).unwrap_or_default();
            self.b.terminate(Term::Jump(Target { to: join, args }));
        }

        // else (or straight to join when absent)
        self.b.switch_to(else_b);
        self.terminated = false;
        match else_ {
            Some(e) => {
                let ev = self.lower_expr(e);
                if !self.terminated {
                    let args = join_param.map(|_| vec![ev]).unwrap_or_default();
                    self.b.terminate(Term::Jump(Target { to: join, args }));
                }
            }
            None => {
                self.b.terminate(Term::Jump(Target { to: join, args: vec![] }));
            }
        }

        self.b.switch_to(join);
        self.terminated = false;
        join_param.unwrap_or_else(|| self.unit(ty))
    }

    /// `try`/`try?`/`try!` and `try ... catch`. The body runs with an error handler
    /// installed; a throwing call or `throw` inside jumps to it. On success the body's
    /// value flows to the join; the handler propagates, softens to null, aborts, or runs
    /// the catch.
    fn lower_try(
        &mut self,
        form: ast::TryForm,
        body: &Expr,
        catch: Option<&ast::CatchArm>,
        repr: Repr,
        ty: TyId,
    ) -> Value {
        let join = self.b.new_block();
        let join_p = self.b.block_param(join, repr.clone(), ty);
        let handler = self.b.new_block();
        let err_param = self.b.block_param(handler, Repr::Any, ty);

        self.handlers.push(handler);
        let body_v = self.lower_expr(body);
        if !self.terminated {
            self.b.terminate(Term::Jump(Target { to: join, args: vec![body_v] }));
        }
        self.handlers.pop();

        self.b.switch_to(handler);
        self.terminated = false;
        if let Some(c) = catch {
            self.scope.push(vec![]);
            self.bind(&c.binding, err_param);
            let cv = self.lower_block(&c.body).unwrap_or_else(|| self.unit(ty));
            if !self.terminated {
                self.b.terminate(Term::Jump(Target { to: join, args: vec![cv] }));
            }
            self.scope.pop();
        } else {
            match form {
                ast::TryForm::Propagate => self.b.terminate(Term::Throw(err_param)),
                ast::TryForm::Soften => {
                    let n = self.b.emit(Op::ConstNull, Repr::Null, ty);
                    self.b.terminate(Term::Jump(Target { to: join, args: vec![n] }));
                }
                ast::TryForm::Assert => {
                    self.b.emit_void(Op::Native {
                        symbol: "neon_panic".into(),
                        args: vec![err_param],
                    });
                    self.b.terminate(Term::Unreachable);
                }
            }
        }

        self.b.switch_to(join);
        self.terminated = false;
        join_p
    }

    /// Wrap a call whose target may throw: check the tagged result and, on error, jump
    /// to the enclosing handler with the error; on success continue with the ok value.
    fn wrap_throwing(&mut self, result: Value, throws_ty: TyId, ok_repr: Repr, ty: TyId) -> Value {
        if matches!(repr_of(&self.env.solver.t, throws_ty), Repr::Never) {
            return result;
        }
        let iserr = self.b.emit(Op::IsErr(result), Repr::Bool, ty);
        let err = self.b.emit(Op::UnwrapErr(result), Repr::Any, ty);
        let ok_b = self.b.new_block();
        match self.handlers.last().copied() {
            Some(h) => self.b.terminate(Term::Branch {
                cond: iserr,
                then: Target { to: h, args: vec![err] },
                els: Target { to: ok_b, args: vec![] },
            }),
            // The checker forbids a bare throwing call, so a handler is always present;
            // defensively, propagate straight out.
            None => self.b.terminate(Term::Branch {
                cond: iserr,
                then: Target { to: ok_b, args: vec![] },
                els: Target { to: ok_b, args: vec![] },
            }),
        }
        self.b.switch_to(ok_b);
        self.terminated = false;
        self.b.emit(Op::UnwrapOk(result), ok_repr, ty)
    }

    /// A `match` as a decision list: each arm tests the subject and, on a match, binds
    /// its pattern and runs its body, jumping to a join with the result. A dense
    /// integer or tag decision list is left for the optimiser to fold into a switch.
    fn lower_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[ast::MatchArm],
        repr: Repr,
        ty: TyId,
    ) -> Value {
        let subj = self.lower_expr(scrutinee);
        let produces = !matches!(repr, Repr::Unit);
        let join = self.b.new_block();
        let join_param = produces.then(|| self.b.block_param(join, repr.clone(), ty));

        for arm in arms {
            let matched = self.b.new_block();
            let next = self.b.new_block();

            // Test the pattern in the current block.
            match self.pattern_test(subj, &arm.pat) {
                None => self.b.terminate(Term::Jump(Target { to: matched, args: vec![] })),
                Some(test) => self.b.terminate(Term::Branch {
                    cond: test,
                    then: Target { to: matched, args: vec![] },
                    els: Target { to: next, args: vec![] },
                }),
            }

            // The matched block binds the pattern, checks any guard, and runs the body.
            self.b.switch_to(matched);
            self.terminated = false;
            self.scope.push(vec![]);
            self.bind_match_pattern(subj, &arm.pat);
            if let Some(g) = &arm.guard {
                let gv = self.lower_expr(g);
                let body_b = self.b.new_block();
                self.b.terminate(Term::Branch {
                    cond: gv,
                    then: Target { to: body_b, args: vec![] },
                    els: Target { to: next, args: vec![] },
                });
                self.b.switch_to(body_b);
                self.terminated = false;
            }
            let bv = self.lower_expr(&arm.body);
            if !self.terminated {
                let args = join_param.map(|_| vec![bv]).unwrap_or_default();
                self.b.terminate(Term::Jump(Target { to: join, args }));
            }
            self.scope.pop();

            self.b.switch_to(next);
            self.terminated = false;
        }

        // The checker proved the arms exhaustive, so falling off the last is unreachable.
        self.b.terminate(Term::Unreachable);
        self.b.switch_to(join);
        self.terminated = false;
        join_param.unwrap_or_else(|| self.unit(ty))
    }

    /// The test an arm's pattern imposes, or `None` when it always matches. Sub-patterns
    /// (a nested literal in a field) contribute their own tests, ANDed together.
    fn pattern_test(&mut self, subj: Value, pat: &ast::Pattern) -> Option<Value> {
        match &pat.kind {
            ast::PatternKind::Wildcard | ast::PatternKind::Bind(_) => None,
            ast::PatternKind::Is(spec) => Some(self.type_test(subj, spec)),
            ast::PatternKind::Literal(lit) => Some(self.literal_test(subj, lit)),
            ast::PatternKind::Record { path, fields, .. } => {
                let mut test = path.as_ref().and_then(|p| p.last()).map(|n| {
                    self.b.emit(
                        Op::IsVariant { value: subj, variant: n.clone() },
                        Repr::Bool,
                        subj_ty(&self.b, subj),
                    )
                });
                for f in fields {
                    if let Some(sub) = &f.pat {
                        let r = field_repr(&self.b, subj, &f.name);
                        let fv = self.b.emit(
                            Op::Field { base: subj, field: f.name.clone() },
                            r,
                            subj_ty(&self.b, subj),
                        );
                        if let Some(sub_test) = self.pattern_test(fv, sub) {
                            test = Some(self.and(test, sub_test));
                        }
                    }
                }
                test
            }
            ast::PatternKind::Tuple(ps) => {
                let mut test = None;
                for (i, sub) in ps.iter().enumerate() {
                    let r = elem_repr(&self.b, subj, i);
                    let ev =
                        self.b.emit(Op::Elem { base: subj, index: i }, r, subj_ty(&self.b, subj));
                    if let Some(sub_test) = self.pattern_test(ev, sub) {
                        test = Some(self.and(test, sub_test));
                    }
                }
                test
            }
            _ => None,
        }
    }

    /// `x is T` as a runtime test: null becomes a null check, anything else a
    /// discriminant compare against the type's head name.
    fn type_test(&mut self, subj: Value, spec: &ast::TypeSpec) -> Value {
        let bty = subj_ty(&self.b, subj);
        match &spec.kind {
            ast::TypeSpecKind::Null => self.b.emit(Op::IsNull(subj), Repr::Bool, bty),
            ast::TypeSpecKind::Named { path, .. } => {
                let variant = path.last().cloned().unwrap_or_default();
                self.b.emit(Op::IsVariant { value: subj, variant }, Repr::Bool, bty)
            }
            ast::TypeSpecKind::Atom(a) => {
                let lit = self.b.emit(Op::ConstAtom(a.clone()), Repr::Tag, bty);
                self.b.emit(Op::Prim(PrimOp::Eq, vec![subj, lit]), Repr::Bool, bty)
            }
            _ => self.b.emit(Op::ConstBool(true), Repr::Bool, bty),
        }
    }

    /// A literal pattern tests equality; a `null` literal is a null check.
    fn literal_test(&mut self, subj: Value, lit: &Expr) -> Value {
        let bty = subj_ty(&self.b, subj);
        if matches!(lit.kind, ExprKind::Null) {
            return self.b.emit(Op::IsNull(subj), Repr::Bool, bty);
        }
        let lv = self.lower_expr(lit);
        self.b.emit(Op::Prim(PrimOp::Eq, vec![subj, lv]), Repr::Bool, bty)
    }

    fn and(&mut self, a: Option<Value>, b: Value) -> Value {
        match a {
            Some(a) => {
                let bty = subj_ty(&self.b, b);
                self.b.emit(Op::Prim(PrimOp::And, vec![a, b]), Repr::Bool, bty)
            }
            None => b,
        }
    }

    /// Bind the names an arm's pattern introduces: a bare binding narrows the subject,
    /// a record/tuple pattern projects and recurses.
    fn bind_match_pattern(&mut self, subj: Value, pat: &ast::Pattern) {
        match &pat.kind {
            ast::PatternKind::Bind(n) => self.bind(n, subj),
            ast::PatternKind::Record { fields, .. } => {
                for f in fields {
                    let r = field_repr(&self.b, subj, &f.name);
                    let fv = self.b.emit(
                        Op::Field { base: subj, field: f.name.clone() },
                        r,
                        subj_ty(&self.b, subj),
                    );
                    match &f.pat {
                        Some(sub) => self.bind_match_pattern(fv, sub),
                        None => self.bind(&f.name, fv),
                    }
                }
            }
            ast::PatternKind::Tuple(ps) => {
                for (i, sub) in ps.iter().enumerate() {
                    let r = elem_repr(&self.b, subj, i);
                    let ev =
                        self.b.emit(Op::Elem { base: subj, index: i }, r, subj_ty(&self.b, subj));
                    self.bind_match_pattern(ev, sub);
                }
            }
            _ => {}
        }
    }

    /// `loop { body }` — an infinite loop the body leaves with `break`. Its value is
    /// the union of the break values (the exit block's parameter). Loop-carried
    /// variables (those the body reassigns) become the header block's parameters, which
    /// is how mutable-looking locals stay SSA.
    fn lower_loop(&mut self, body: &Block, repr: Repr, ty: TyId) -> Value {
        let carried = self.carried_vars(body);
        let inits: Vec<Value> = carried.iter().map(|n| self.lookup(n).unwrap()).collect();

        let header = self.b.new_block();
        let exit = self.b.new_block();
        let produces = !matches!(repr, Repr::Unit);
        let exit_param = produces.then(|| self.b.block_param(exit, repr.clone(), ty));

        // Header parameters mirror the carried variables' current reprs.
        let mut header_params = Vec::new();
        for &v in &inits {
            let (r, vty) = (self.b.value_repr(v).clone(), self.b.value_ty(v));
            header_params.push(self.b.block_param(header, r, vty));
        }

        self.b.terminate(Term::Jump(Target { to: header, args: inits }));
        self.b.switch_to(header);
        self.terminated = false;
        self.scope.push(vec![]);
        for (n, &p) in carried.iter().zip(&header_params) {
            self.bind(n, p);
        }

        self.loops.push(LoopCtx { continue_target: header, exit, carried: carried.clone(), has_value: produces });
        for s in &body.stmts {
            if self.terminated {
                break;
            }
            self.lower_stmt(s);
        }
        if !self.terminated {
            if let Some(t) = &body.tail {
                self.lower_expr(t);
            }
        }
        // The back-edge: loop around with the carried variables' latest values.
        if !self.terminated {
            self.jump_to_header();
        }
        self.loops.pop();
        self.scope.pop();

        self.b.switch_to(exit);
        self.terminated = false;
        exit_param.unwrap_or_else(|| self.unit(ty))
    }

    /// `while cond { body }` — a loop whose header tests the condition. Yields unit.
    fn lower_while(&mut self, cond: &Expr, body: &Block, ty: TyId) -> Value {
        let carried = self.carried_vars(body);
        let inits: Vec<Value> = carried.iter().map(|n| self.lookup(n).unwrap()).collect();

        let header = self.b.new_block();
        let body_b = self.b.new_block();
        let exit = self.b.new_block();

        let mut header_params = Vec::new();
        for &v in &inits {
            let (r, vty) = (self.b.value_repr(v).clone(), self.b.value_ty(v));
            header_params.push(self.b.block_param(header, r, vty));
        }

        self.b.terminate(Term::Jump(Target { to: header, args: inits }));
        self.b.switch_to(header);
        self.terminated = false;
        self.scope.push(vec![]);
        for (n, &p) in carried.iter().zip(&header_params) {
            self.bind(n, p);
        }
        let cond_v = self.lower_expr(cond);
        self.b.terminate(Term::Branch {
            cond: cond_v,
            then: Target { to: body_b, args: vec![] },
            els: Target { to: exit, args: vec![] },
        });

        self.b.switch_to(body_b);
        self.terminated = false;
        self.loops.push(LoopCtx { continue_target: header, exit, carried: carried.clone(), has_value: false });
        for s in &body.stmts {
            if self.terminated {
                break;
            }
            self.lower_stmt(s);
        }
        if !self.terminated {
            self.jump_to_header();
        }
        self.loops.pop();
        self.scope.pop();

        self.b.switch_to(exit);
        self.terminated = false;
        self.unit(ty)
    }

    /// `for x in xs { body }` — a C loop over a contiguous list, indexed from 0 to its
    /// length. The index and any reassigned locals are block parameters; the latch block
    /// increments the index and is where `continue` lands.
    fn lower_for(&mut self, pat: &ast::Pattern, iter: &Expr, body: &Block, ty: TyId) -> Value {
        let list = self.lower_expr(iter);
        let elem_repr = match self.b.value_repr(list) {
            Repr::List(e) => (**e).clone(),
            _ => Repr::Any,
        };
        let len = self.b.emit(Op::Native { symbol: "neon_list_len".into(), args: vec![list] }, Repr::I64, ty);
        let zero = self.b.emit(Op::ConstI64(0), Repr::I64, ty);

        let carried = self.carried_vars(body);
        let inits: Vec<Value> = carried.iter().map(|n| self.lookup(n).unwrap()).collect();

        let header = self.b.new_block();
        let body_b = self.b.new_block();
        let latch = self.b.new_block();
        let exit = self.b.new_block();

        let i_param = self.b.block_param(header, Repr::I64, ty);
        let mut carried_params = Vec::new();
        for &v in &inits {
            let (r, vty) = (self.b.value_repr(v).clone(), self.b.value_ty(v));
            carried_params.push(self.b.block_param(header, r, vty));
        }
        // The latch takes the carried variables from each back-edge (body end, continue).
        let mut latch_params = Vec::new();
        for &v in &inits {
            let (r, vty) = (self.b.value_repr(v).clone(), self.b.value_ty(v));
            latch_params.push(self.b.block_param(latch, r, vty));
        }

        let mut entry_args = vec![zero];
        entry_args.extend(inits);
        self.b.terminate(Term::Jump(Target { to: header, args: entry_args }));

        // header: test the index, bind carried, branch into the body or out.
        self.b.switch_to(header);
        self.terminated = false;
        self.scope.push(vec![]);
        for (n, &p) in carried.iter().zip(&carried_params) {
            self.bind(n, p);
        }
        let cond = self.b.emit(Op::Prim(PrimOp::Lt, vec![i_param, len]), Repr::Bool, ty);
        self.b.terminate(Term::Branch {
            cond,
            then: Target { to: body_b, args: vec![] },
            els: Target { to: exit, args: vec![] },
        });

        // body: bind the element and run.
        self.b.switch_to(body_b);
        self.terminated = false;
        self.scope.push(vec![]);
        let elem = self.b.emit(Op::Index { base: list, index: i_param }, elem_repr, ty);
        self.bind_pattern(pat, elem);
        self.loops.push(LoopCtx {
            continue_target: latch,
            exit,
            carried: carried.clone(),
            has_value: false,
        });
        for s in &body.stmts {
            if self.terminated {
                break;
            }
            self.lower_stmt(s);
        }
        if !self.terminated {
            if let Some(t) = &body.tail {
                self.lower_expr(t);
            }
        }
        if !self.terminated {
            let args: Vec<Value> = carried.iter().map(|n| self.lookup(n).unwrap()).collect();
            self.b.terminate(Term::Jump(Target { to: latch, args }));
        }
        self.loops.pop();
        self.scope.pop();

        // latch: increment the index, loop with the carried variables it received.
        self.b.switch_to(latch);
        self.terminated = false;
        let one = self.b.emit(Op::ConstI64(1), Repr::I64, ty);
        let next_i = self.b.emit(Op::Prim(PrimOp::Add, vec![i_param, one]), Repr::I64, ty);
        let mut back = vec![next_i];
        back.extend(latch_params);
        self.b.terminate(Term::Jump(Target { to: header, args: back }));

        self.scope.pop();
        self.b.switch_to(exit);
        self.terminated = false;
        self.unit(ty)
    }

    /// Jump to the innermost loop's header with the current values of its carried vars.
    fn jump_to_header(&mut self) {
        if let Some(ctx) = self.loops.last() {
            let (target, carried) = (ctx.continue_target, ctx.carried.clone());
            let args: Vec<Value> = carried.iter().map(|n| self.lookup(n).unwrap()).collect();
            self.b.terminate(Term::Jump(Target { to: target, args }));
        }
    }

    /// The variables a loop body reassigns that are bound outside it — the loop-carried
    /// state. Nested loops and lambdas manage their own, so the scan does not descend
    /// into them.
    fn carried_vars(&self, body: &Block) -> Vec<String> {
        let mut names = Vec::new();
        collect_assigns_block(body, &mut names);
        names.retain(|n| self.lookup(n).is_some());
        let mut seen = std::collections::HashSet::new();
        names.retain(|n| seen.insert(n.clone()));
        names
    }

    // ---- helpers ----

    fn unit(&mut self, ty: TyId) -> Value {
        self.b.emit(Op::ConstUnit, Repr::Unit, ty)
    }

    /// A not-yet-lowered expression: emits a placeholder of the right repr so the rest
    /// of the function still lowers, and panics loudly in tests via the note. During
    /// bring-up these mark exactly what remains.
    fn unhandled(&mut self, e: &Expr, repr: Repr, ty: TyId) -> Value {
        self.unhandled_note(kind_name(&e.kind), repr, ty)
    }

    fn unhandled_note(&mut self, what: &str, repr: Repr, ty: TyId) -> Value {
        self.b.emit(Op::ConstStr(format!("<todo: {what}>")), repr, ty)
    }
}

fn kind_name(k: &ExprKind) -> &'static str {
    match k {
        ExprKind::Match { .. } => "match",
        ExprKind::List(_) => "list literal",
        ExprKind::RecordLit { .. } => "record literal",
        ExprKind::Tuple(_) => "tuple",
        ExprKind::Lambda { .. } => "lambda",
        ExprKind::Loop { .. } => "loop",
        ExprKind::While { .. } => "while",
        ExprKind::For { .. } => "for",
        ExprKind::Break(_) => "break",
        ExprKind::Continue => "continue",
        ExprKind::Throw(_) => "throw",
        ExprKind::Try { .. } => "try",
        ExprKind::Is { .. } => "is",
        ExprKind::As { .. } => "as",
        ExprKind::Index { .. } => "index",
        ExprKind::Field { .. } => "field",
        ExprKind::Assert { .. } => "assert",
        _ => "expr",
    }
}

fn subj_ty(b: &Builder, v: Value) -> TyId {
    b.value_ty(v)
}

/// The runtime `to_string` symbol for a primitive repr, for string interpolation. A
/// `str` needs none (identity); a user type needs a Display dispatch instead.
fn to_string_symbol(r: &Repr) -> Option<String> {
    Some(match r {
        Repr::I64 => "neon_i64_to_string",
        Repr::F64 => "neon_f64_to_string",
        Repr::Bool => "neon_bool_to_string",
        _ => return None,
    }
    .to_string())
}

// ---- scanning for a lambda's free variables ----

fn pattern_names(p: &ast::Pattern, out: &mut Vec<String>) {
    match &p.kind {
        ast::PatternKind::Bind(n) => out.push(n.clone()),
        ast::PatternKind::Tuple(ps) => ps.iter().for_each(|s| pattern_names(s, out)),
        ast::PatternKind::Record { fields, .. } => {
            for f in fields {
                match &f.pat {
                    Some(sub) => pattern_names(sub, out),
                    None => out.push(f.name.clone()),
                }
            }
        }
        _ => {}
    }
}

fn collect_free(block: &Block, bound: &mut std::collections::HashSet<String>, used: &mut Vec<String>) {
    for s in &block.stmts {
        match &s.kind {
            StmtKind::Let { pat, value, .. } => {
                collect_free_expr(value, bound, used);
                let mut names = Vec::new();
                pattern_names(pat, &mut names);
                bound.extend(names);
            }
            StmtKind::Assign { value, .. } => collect_free_expr(value, bound, used),
            StmtKind::Expr(e) => collect_free_expr(e, bound, used),
            StmtKind::Error => {}
        }
    }
    if let Some(t) = &block.tail {
        collect_free_expr(t, bound, used);
    }
}

fn collect_free_expr(
    e: &Expr,
    bound: &mut std::collections::HashSet<String>,
    used: &mut Vec<String>,
) {
    match &e.kind {
        ExprKind::Path(p) => {
            if let [name] = p.as_slice() {
                if !bound.contains(name) {
                    used.push(name.clone());
                }
            }
        }
        ExprKind::Lambda { params, body } => {
            // A nested lambda's own parameters are bound within it; do not leak them out.
            let mut inner = bound.clone();
            inner.extend(params.iter().map(|p| p.name.clone()));
            collect_free_expr(body, &mut inner, used);
        }
        ExprKind::Unary { rhs, .. } => collect_free_expr(rhs, bound, used),
        ExprKind::Binary { lhs, rhs, .. } => {
            collect_free_expr(lhs, bound, used);
            collect_free_expr(rhs, bound, used);
        }
        ExprKind::Call { callee, args, .. } => {
            collect_free_expr(callee, bound, used);
            args.iter().for_each(|a| collect_free_expr(a, bound, used));
        }
        ExprKind::Index { base, index } => {
            collect_free_expr(base, bound, used);
            collect_free_expr(index, bound, used);
        }
        ExprKind::Field { base, .. } => collect_free_expr(base, bound, used),
        ExprKind::List(elems) => elems.iter().for_each(|el| match el {
            ast::Elem::Value(x) | ast::Elem::Spread(x) => collect_free_expr(x, bound, used),
        }),
        ExprKind::RecordLit { fields, spread, .. } => {
            fields.iter().for_each(|f| collect_free_expr(&f.value, bound, used));
            if let Some(s) = spread {
                collect_free_expr(s, bound, used);
            }
        }
        ExprKind::Tuple(es) => es.iter().for_each(|x| collect_free_expr(x, bound, used)),
        ExprKind::If { cond, then, else_ } => {
            collect_free_expr(cond, bound, used);
            collect_free(then, bound, used);
            if let Some(x) = else_ {
                collect_free_expr(x, bound, used);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            collect_free_expr(scrutinee, bound, used);
            for arm in arms {
                // Arm-pattern bindings scope to the arm; over-approximate by adding them
                // to `bound` (a capture named the same as an arm binding is rare).
                let mut names = Vec::new();
                pattern_names(&arm.pat, &mut names);
                let mut inner = bound.clone();
                inner.extend(names);
                if let Some(g) = &arm.guard {
                    collect_free_expr(g, &mut inner, used);
                }
                collect_free_expr(&arm.body, &mut inner, used);
            }
        }
        ExprKind::Block(b) => collect_free(b, bound, used),
        ExprKind::Loop { body } => collect_free(body, bound, used),
        ExprKind::While { cond, body } => {
            collect_free_expr(cond, bound, used);
            collect_free(body, bound, used);
        }
        ExprKind::For { pat, iter, body } => {
            collect_free_expr(iter, bound, used);
            let mut names = Vec::new();
            pattern_names(pat, &mut names);
            let mut inner = bound.clone();
            inner.extend(names);
            collect_free(body, &mut inner, used);
        }
        ExprKind::Break(Some(x)) | ExprKind::Return(Some(x)) | ExprKind::Throw(x) => {
            collect_free_expr(x, bound, used)
        }
        ExprKind::Try { body, catch, .. } => {
            collect_free_expr(body, bound, used);
            if let Some(c) = catch {
                let mut inner = bound.clone();
                inner.insert(c.binding.clone());
                collect_free(&c.body, &mut inner, used);
            }
        }
        ExprKind::Is { lhs, .. } | ExprKind::As { lhs, .. } => collect_free_expr(lhs, bound, used),
        ExprKind::Assert { args, .. } => args.iter().for_each(|a| collect_free_expr(a, bound, used)),
        _ => {}
    }
}

// ---- scanning for a loop's reassigned variables ----
//
// Descends into every sub-expression except a lambda's body: a closure cannot reassign
// a capture (captures are sealed), so it never contributes to a loop's carried set.

fn collect_assigns_block(b: &Block, out: &mut Vec<String>) {
    for s in &b.stmts {
        match &s.kind {
            StmtKind::Let { value, .. } => collect_assigns_expr(value, out),
            StmtKind::Assign { name, value } => {
                out.push(name.clone());
                collect_assigns_expr(value, out);
            }
            StmtKind::Expr(e) => collect_assigns_expr(e, out),
            StmtKind::Error => {}
        }
    }
    if let Some(t) = &b.tail {
        collect_assigns_expr(t, out);
    }
}

fn collect_assigns_expr(e: &Expr, out: &mut Vec<String>) {
    match &e.kind {
        ExprKind::Unary { rhs, .. } => collect_assigns_expr(rhs, out),
        ExprKind::Binary { lhs, rhs, .. } => {
            collect_assigns_expr(lhs, out);
            collect_assigns_expr(rhs, out);
        }
        ExprKind::Call { callee, args, .. } => {
            collect_assigns_expr(callee, out);
            args.iter().for_each(|a| collect_assigns_expr(a, out));
        }
        ExprKind::Index { base, index } => {
            collect_assigns_expr(base, out);
            collect_assigns_expr(index, out);
        }
        ExprKind::Field { base, .. } => collect_assigns_expr(base, out),
        ExprKind::List(elems) => {
            for el in elems {
                match el {
                    ast::Elem::Value(x) | ast::Elem::Spread(x) => collect_assigns_expr(x, out),
                }
            }
        }
        ExprKind::RecordLit { fields, spread, .. } => {
            fields.iter().for_each(|f| collect_assigns_expr(&f.value, out));
            if let Some(s) = spread {
                collect_assigns_expr(s, out);
            }
        }
        ExprKind::Tuple(es) => es.iter().for_each(|x| collect_assigns_expr(x, out)),
        ExprKind::If { cond, then, else_ } => {
            collect_assigns_expr(cond, out);
            collect_assigns_block(then, out);
            if let Some(e) = else_ {
                collect_assigns_expr(e, out);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            collect_assigns_expr(scrutinee, out);
            for arm in arms {
                if let Some(g) = &arm.guard {
                    collect_assigns_expr(g, out);
                }
                collect_assigns_expr(&arm.body, out);
            }
        }
        ExprKind::Block(b) => collect_assigns_block(b, out),
        ExprKind::Loop { body } => collect_assigns_block(body, out),
        ExprKind::While { cond, body } => {
            collect_assigns_expr(cond, out);
            collect_assigns_block(body, out);
        }
        ExprKind::For { iter, body, .. } => {
            collect_assigns_expr(iter, out);
            collect_assigns_block(body, out);
        }
        ExprKind::Break(Some(e)) | ExprKind::Return(Some(e)) | ExprKind::Throw(e) => {
            collect_assigns_expr(e, out)
        }
        ExprKind::Try { body, catch, .. } => {
            collect_assigns_expr(body, out);
            if let Some(c) = catch {
                collect_assigns_block(&c.body, out);
            }
        }
        ExprKind::Is { lhs, .. } | ExprKind::As { lhs, .. } => collect_assigns_expr(lhs, out),
        ExprKind::Assert { args, .. } => args.iter().for_each(|a| collect_assigns_expr(a, out)),
        // A lambda's body is a separate scope that cannot reassign a capture.
        _ => {}
    }
}

fn elem_repr(b: &Builder, base: Value, index: usize) -> Repr {
    match b.value_repr(base) {
        Repr::Tuple(rs) => rs.get(index).cloned().unwrap_or(Repr::Unit),
        _ => Repr::Unit,
    }
}

fn field_repr(b: &Builder, base: Value, field: &str) -> Repr {
    match b.value_repr(base) {
        Repr::Record { fields, .. } => {
            fields.iter().find(|(n, _)| n == field).map(|(_, r)| r.clone()).unwrap_or(Repr::Unit)
        }
        _ => Repr::Unit,
    }
}

fn un_prim(op: UnOp) -> PrimOp {
    match op {
        UnOp::Neg => PrimOp::Neg,
        UnOp::Not => PrimOp::Not,
        UnOp::Bnot => PrimOp::Bnot,
    }
}

fn bin_prim(op: BinOp) -> Option<PrimOp> {
    Some(match op {
        BinOp::Add => PrimOp::Add,
        BinOp::Sub => PrimOp::Sub,
        BinOp::Mul => PrimOp::Mul,
        BinOp::Div => PrimOp::Div,
        BinOp::Rem => PrimOp::Rem,
        BinOp::Eq => PrimOp::Eq,
        BinOp::Ne => PrimOp::Ne,
        BinOp::Lt => PrimOp::Lt,
        BinOp::Le => PrimOp::Le,
        BinOp::Gt => PrimOp::Gt,
        BinOp::Ge => PrimOp::Ge,
        BinOp::Band => PrimOp::Band,
        BinOp::Bor => PrimOp::Bor,
        BinOp::Bxor => PrimOp::Bxor,
        BinOp::Bsl => PrimOp::Bsl,
        BinOp::Bsr => PrimOp::Bsr,
        // Short-circuit and null/pipe forms desugar; not a plain prim.
        BinOp::And | BinOp::Or | BinOp::Orelse | BinOp::Pipe => return None,
    })
}

#[cfg(test)]
mod tests;
