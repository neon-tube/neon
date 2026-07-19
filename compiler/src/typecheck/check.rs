//! The checker: a type for every expression.
//!
//! Bidirectional. `expected` flows down where a form can use it — a list's
//! elements, a lambda's parameters, an `if`'s arms — and types flow up everywhere
//! else. Where both meet, one rule decides: `actual <: expected`.
//!
//! Nothing here may invent a type when it does not know one. There is no `Erased`
//! to fall back to and no way to write one; when the checker cannot work something
//! out it emits a diagnostic and poisons that expression, so the cascade is one
//! error rather than twenty silent ones.

use super::dispatch::{self, DispatchError};
use super::env::{Env, TypeError, TypeErrorKind};
use super::narrow::{self, Projected};
use super::print::print;
use super::resolve::Scope;
use super::result::TypecheckResult;
use super::types::TyId;
use crate::ast::{self, BinOp, Expr, ExprKind, UnOp};
use crate::lexer::Span;

pub fn check_module(env: &mut Env, m: &ast::Module) -> (TypecheckResult, Vec<TypeError>) {
    check_all(env, &[(Vec::new(), m)])
}

/// Check every module of a compilation, accumulating into one `TypecheckResult`.
///
/// The stdlib is checked here too, at its own module path: its function bodies are real
/// Neon code that has to be lowered, and lowering reads types and call resolutions out of
/// this result. Ids are unique across modules (see `ast::number_exprs_from`), so one map
/// covers them all.
pub fn check_all(
    env: &mut Env,
    modules: &[(Vec<String>, &ast::Module)],
) -> (TypecheckResult, Vec<TypeError>) {
    let mut c = Checker {
        env,
        result: TypecheckResult::default(),
        errors: vec![],
        locals: vec![],
        ret: None,
        throws: None,
        loop_breaks: vec![],
        throw_sinks: vec![],
        bounds: vec![],
        rigids: vec![],
        lambda_returns: vec![],
        lambda_throws: vec![],
        capture_floors: vec![],
    };
    for (path, m) in modules {
        c.decls(path, &m.decls);
    }
    // One mistake, one diagnostic. A generic call checks each argument twice -- once while
    // solving the callee's type parameters, then again under the solution, which is what
    // lets an expected type flow into a lambda argument -- so anything wrong *inside* an
    // argument was reported twice. Deduplicating the finished list is cheaper than
    // threading a "probing, stay quiet" mode through every expression form, and an
    // identical kind at an identical span is the same mistake by construction.
    let mut seen = Vec::new();
    c.errors.retain(|e| {
        let key = (e.span.clone(), e.kind.clone());
        if seen.contains(&key) {
            return false;
        }
        seen.push(key);
        true
    });
    (c.result, c.errors)
}

/// What the enclosing function may fail with. A declared clause is a *type*, checked by
/// subtyping. `main`'s implicit channel is not a type at all — substituting ⊤ for it both
/// erased the error path and, because everything is a subtype of ⊤, silently switched off
/// the check that a thrown value is an error. It is a rule instead: whatever escapes must
/// implement `Error`, checked per throw site.
#[derive(Clone, Copy)]
enum Throws {
    Declared(TyId),
    ImplicitError,
    /// A lambda body. There is no syntax to declare a lambda's `throws` — like its
    /// return type, it is an output, derivable from the body — so whatever propagates
    /// out is collected (in `lambda_throws`) instead of checked against a clause.
    Infer,
}

struct Checker<'a> {
    env: &'a mut Env,
    result: TypecheckResult,
    errors: Vec<TypeError>,
    /// Innermost last. A name resolves to the nearest binding. Each carries the span
    /// it was bound at, so a diagnostic can point back at a name's origin.
    locals: Vec<Vec<(String, TyId, Span)>>,
    ret: Option<TyId>,
    throws: Option<Throws>,
    /// Break values of the enclosing loops, innermost last. A `loop` is the union
    /// of the values it breaks with.
    loop_breaks: Vec<Vec<TyId>>,
    /// Throws collected by the enclosing `try` bodies. A throwing call outside any
    /// `try` is a compile error; inside one, its error type lands here.
    throw_sinks: Vec<Vec<TyId>>,
    /// The current function's `where T: P` bounds, as (param name, protocol). A
    /// method call on a rigid `T` is only allowed to resolve through one of these.
    bounds: Vec<(String, super::env::ProtocolId)>,
    /// The current function's generic names, so a type written in its body -- `as T`,
    /// `is T`, `let x: T` -- resolves `T` as the rigid variable it introduced.
    rigids: Vec<String>,
    /// One frame per enclosing lambda, collecting the types its `return`s produce. A
    /// lambda declares no return type, so its type is the union of its tail and these.
    lambda_returns: Vec<Vec<TyId>>,
    /// One frame per enclosing lambda, collecting the error types that propagate out
    /// of it — a `throw` or a `try`-propagate in its body. A lambda declares no
    /// `throws` either; the union of these is the clause its arrow gets.
    lambda_throws: Vec<Vec<TyId>>,
    /// One entry per enclosing lambda: the `locals` depth where that lambda's own
    /// scope begins. A name found in a frame below the innermost floor was captured,
    /// and assigning to a capture is an error -- the closure holds a private copy.
    capture_floors: Vec<usize>,
}

impl Checker<'_> {
    fn error(&mut self, span: Span, kind: TypeErrorKind) {
        self.errors.push(TypeError { span, kind });
    }

    fn show(&mut self, t: TyId) -> String {
        print(&mut self.env.solver.t, t)
    }

    fn poison(&mut self) -> TyId {
        self.env.error_ty()
    }

    /// Union of two branch types, absorbing poison. A branch that already produced
    /// a diagnostic must not make the whole `if`/`match` a `T | #error` that then
    /// fails to match its expected type -- one mistake, one error.
    fn union_branches(&mut self, a: TyId, b: TyId) -> TyId {
        if self.env.is_error(a) || self.env.is_error(b) {
            return self.poison();
        }
        self.env.solver.t.union(a, b)
    }

    /// `actual <: expected`, unless either is already poison — a checked
    /// expression that already produced a diagnostic must not produce a second.
    fn assignable(&mut self, actual: TyId, expected: TyId) -> bool {
        if self.env.is_error(actual) || self.env.is_error(expected) {
            return true;
        }
        self.env.solver.is_subtype(actual, expected)
    }

    // ---- declarations ----

    fn decls(&mut self, module: &[String], decls: &[ast::Decl]) {
        for d in decls {
            match &d.kind {
                ast::DeclKind::Fn(f) => {
                    // `main`'s fixed signature is enforced in the declaration phase
                    // (`Env::fn_sig`), so an illegal clause is caught even when it
                    // would not resolve as a type.
                    self.fn_body(module, f, &[]);
                }
                ast::DeclKind::Impl(i) => {
                    for m in &i.methods {
                        self.fn_body(module, m, &i.generics);
                    }
                }
                ast::DeclKind::Protocol(p) => {
                    for m in &p.methods {
                        if m.body.is_some() {
                            self.fn_body(module, m, &[]);
                        }
                    }
                }
                ast::DeclKind::Mod(m) => {
                    let mut inner = module.to_vec();
                    inner.push(m.name.clone());
                    self.decls(&inner, &m.decls);
                }
                ast::DeclKind::TestBlock(t) => {
                    self.locals.push(vec![]);
                    let never = self.env.solver.t.never();
                    self.ret = Some(never);
                    self.throws = Some(Throws::Declared(never));
                    self.block(module, &t.body, None);
                    self.locals.pop();
                }
                _ => {}
            }
        }
    }

    fn fn_body(&mut self, module: &[String], f: &ast::FnDecl, outer: &[String]) {
        let Some(body) = &f.body else { return };

        let mut scope = Scope::new(module);
        let mut generics: Vec<String> = outer.to_vec();
        generics.extend(f.generics.iter().cloned());
        scope = scope.with_rigid(self.env, &generics);
        self.rigids = generics;

        self.locals.push(vec![]);
        for p in &f.params {
            let t = self.env.resolve(&scope, &p.ty);
            self.bind(&p.name, t, p.span.clone());
        }

        let ret = match &f.ret {
            Some(t) => self.env.resolve(&scope, t),
            None => self.env.solver.t.tuple(vec![]),
        };
        let throws = match &f.throws {
            Some(t) => Throws::Declared(self.env.resolve(&scope, t)),
            // `main`'s channel is a rule, not a type: whatever escapes must implement
            // `Error`, because `main` has to report it.
            None if module.is_empty() && f.name == "main" => Throws::ImplicitError,
            None => Throws::Declared(self.env.solver.t.never()),
        };
        self.ret = Some(ret);
        self.throws = Some(throws);
        self.bounds = f
            .wheres
            .iter()
            .filter_map(|w| match &w.bound.kind {
                ast::TypeSpecKind::Named { path, .. } => {
                    self.env.lookup_protocol(module, path).map(|p| (w.param.clone(), p))
                }
                _ => None,
            })
            .collect();

        // A body-less `-> ()` fn is a statement sequence; anything else must
        // produce its return type as the tail.
        let unit = self.env.solver.t.tuple(vec![]);
        let want = if ret == unit { None } else { Some(ret) };
        self.block(module, body, want);
        self.locals.pop();
    }

    // ---- scopes ----

    fn bind(&mut self, name: &str, t: TyId, span: Span) {
        if let Some(scope) = self.locals.last_mut() {
            scope.push((name.to_string(), t, span));
        }
    }

    fn lookup(&self, name: &str) -> Option<TyId> {
        self.locals.iter().rev().flat_map(|s| s.iter().rev()).find(|(n, ..)| n == name).map(|(_, t, _)| *t)
    }

    /// The index of the innermost `locals` frame that binds `name`, for deciding
    /// whether it lies below a lambda's capture floor.
    fn frame_of(&self, name: &str) -> Option<usize> {
        self.locals.iter().enumerate().rev().find(|(_, s)| s.iter().any(|(n, ..)| n == name)).map(|(i, _)| i)
    }

    /// The span where `name` was bound, for a "captured here"-style secondary label.
    fn origin_of(&self, name: &str) -> Option<Span> {
        self.locals.iter().rev().flat_map(|s| s.iter().rev()).find(|(n, ..)| n == name).map(|(.., s)| s.clone())
    }

    // ---- blocks and statements ----

    fn block(&mut self, module: &[String], b: &ast::Block, expected: Option<TyId>) -> TyId {
        self.locals.push(vec![]);
        for s in &b.stmts {
            self.stmt(module, s);
        }
        let t = match &b.tail {
            Some(e) => self.expr(module, e, expected),
            None => self.env.solver.t.tuple(vec![]),
        };
        self.locals.pop();
        t
    }

    /// A bare `if` (no `else`) has no value, so it cannot fill a value position
    /// whose expected type is unknown -- a binding without an annotation, or an
    /// argument to a protocol method. Where the expected type is known, `if_expr`
    /// rejects it against that type instead.
    /// True when `nt` is a newtype whose representation meets `other` -- so a cast
    /// between the two only wraps or unwraps. A newtype carries its representation as
    /// a hidden `#inner` field, a label no source can write, so its presence marks a
    /// newtype and its type is the representation.
    fn newtype_bridges(&mut self, nt: TyId, other: TyId) -> bool {
        let label = self.env.solver.t.name("#inner");
        let Some(inner) = narrow::project_field(&mut self.env.solver, nt, label).ty() else {
            return false;
        };
        let meet = self.env.solver.t.intersect(inner, other);
        !self.env.solver.is_empty(meet)
    }

    fn reject_bare_if(&mut self, e: &Expr) {
        if let ExprKind::If { else_: None, .. } = &e.kind {
            self.error(e.span.clone(), TypeErrorKind::IfWithoutElse);
        }
    }

    /// A scope for resolving a type written in the current function's body. It
    /// carries the function's generics, so `as T` and `let x: T` see `T`.
    fn type_scope(&mut self, module: &[String]) -> Scope {
        let rigids = self.rigids.clone();
        Scope::new(module).with_rigid(self.env, &rigids)
    }

    fn stmt(&mut self, module: &[String], s: &ast::Stmt) {
        match &s.kind {
            ast::StmtKind::Let { pat, ty, value } => {
                let scope = self.type_scope(module);
                let want = ty.as_ref().map(|t| self.env.resolve(&scope, t));
                // A binding consumes a value. With an annotation, `if_expr` already
                // rejects a bare `if` against it; without one, there is no expected
                // type to catch it, so say so here.
                if want.is_none() {
                    self.reject_bare_if(value);
                }
                let t = self.expr(module, value, want);
                // The annotation is the binding's type when there is one: `let x:
                // i64|str = 1` binds the wider type, not `i64`. Record it against the
                // initialiser so lowering lays the binding out at the declared type too --
                // it sees only the initialiser, whose type is the narrow one.
                if let Some(w) = want {
                    self.result.set_declared(value.id, w);
                }
                self.bind_pattern(module, pat, want.unwrap_or(t));
            }
            ast::StmtKind::Assign { name, value } => {
                let Some(want) = self.lookup(name) else {
                    self.error(s.span.clone(), TypeErrorKind::UnknownName(name.clone()));
                    self.expr(module, value, None);
                    return;
                };
                // A capture is immutable inside the closure: assigning to it would
                // write to the closure's private copy, invisible to everyone else.
                if let (Some(&floor), Some(frame)) = (self.capture_floors.last(), self.frame_of(name))
                {
                    if frame < floor {
                        let origin = self.origin_of(name);
                        self.error(
                            s.span.clone(),
                            TypeErrorKind::RebindCapture { name: name.clone(), origin },
                        );
                    }
                }
                self.expr(module, value, Some(want));
            }
            ast::StmtKind::Expr(e) => {
                self.expr(module, e, None);
            }
            ast::StmtKind::Error => {}
        }
    }

    fn bind_pattern(&mut self, module: &[String], p: &ast::Pattern, t: TyId) {
        match &p.kind {
            ast::PatternKind::Bind(n) => self.bind(n, t, p.span.clone()),
            ast::PatternKind::Wildcard => {}
            ast::PatternKind::Tuple(ps) => {
                for (i, sub) in ps.iter().enumerate() {
                    let e = narrow::project_elem(&mut self.env.solver, t, i);
                    let et = self.projected(sub.span.clone(), e, &i.to_string(), t);
                    self.bind_pattern(module, sub, et);
                }
            }
            ast::PatternKind::Record { fields, path, .. } => {
                // Destructuring is a field read that looks like a binding. The name comes
                // from the pattern's own path when it has one, and otherwise from the type
                // being matched.
                match path {
                    Some(q) => self.check_opaque_path(
                        module, p.span.clone(), q, "it can be destructured"),
                    None => if let Some(n) = self.nominal_of(t) {
                        self.check_opaque_name(
                            module, p.span.clone(), &n, "it can be destructured")
                    },
                }
                for f in fields {
                    let label = self.env.solver.t.name(&f.name);
                    let pj = narrow::project_field(&mut self.env.solver, t, label);
                    let ft = self.projected(p.span.clone(), pj, &f.name, t);
                    match &f.pat {
                        Some(sub) => self.bind_pattern(module, sub, ft),
                        None => self.bind(&f.name, ft, f.span.clone()),
                    }
                }
            }
            ast::PatternKind::Is(_) => {}
            ast::PatternKind::Literal(_) | ast::PatternKind::Error => {}
        }
    }

    /// A projection's type, or a diagnostic. `Absent` carries no type on purpose:
    /// `never` would check vacuously against whatever the field went on to be used
    /// as, which is the trap this whole design keeps walking into.
    fn projected(&mut self, span: Span, p: Projected, label: &str, base: TyId) -> TyId {
        match p {
            Projected::Present(t) => t,
            // decisions.md has a missing field satisfy a nullable one, so an optional
            // field reads as `T | null` rather than as an error here.
            Projected::Partial(t) => {
                let null = self.env.solver.t.null();
                self.env.solver.t.union(t, null)
            }
            Projected::Absent => {
                if !self.env.is_error(base) {
                    let on = self.show(base);
                    self.error(span, TypeErrorKind::NoField { field: label.to_string(), on });
                }
                self.poison()
            }
        }
    }

    // ---- expressions ----

    fn expr(&mut self, module: &[String], e: &Expr, expected: Option<TyId>) -> TyId {
        let t = self.infer(module, e, expected);
        if let Some(want) = expected {
            if !self.assignable(t, want) {
                let (found, expect) = (self.show(t), self.show(want));
                self.error(e.span.clone(), TypeErrorKind::Mismatch { expected: expect, found });
            } else if let (Some(a), Some(w)) = (
                self.env.solver.t.as_arrow(t),
                self.env.solver.t.as_arrow(want),
            ) {
                // Subtyping admits a function that throws less where one that throws
                // more is expected — but the `throws` clause is part of the calling
                // convention (a throwing closure returns a tagged result), and the
                // backend has no adapter between the two conventions: `coerce` passes a
                // `neon_closure` through unchanged, so the mismatch would compile clean
                // and read garbage. Until an adapter exists, require the clauses to
                // agree. Lambdas adopt the expected clause at creation, so this bites
                // only a previously-bound value flowing into a more-throwing slot.
                let same = self.env.solver.is_subtype(a.throws, w.throws)
                    && self.env.solver.is_subtype(w.throws, a.throws);
                if !same && !self.env.is_error(a.throws) && !self.env.is_error(w.throws) {
                    let (found, expected) = (self.show(a.throws), self.show(w.throws));
                    self.error(
                        e.span.clone(),
                        TypeErrorKind::ArrowThrowsMismatch { expected, found },
                    );
                }
            }
        }
        self.result.set_ty(e.id, t);
        t
    }

    fn infer(&mut self, module: &[String], e: &Expr, expected: Option<TyId>) -> TyId {
        match &e.kind {
            ExprKind::Int(_) => self.env.solver.t.i64(),
            ExprKind::Float(_) => self.env.solver.t.f64(),
            ExprKind::Bool(_) => self.env.solver.t.bool(),
            ExprKind::Null => self.env.solver.t.null(),
            ExprKind::Rune(_) => self.env.solver.t.i64(),
            ExprKind::Atom(a) => {
                let n = self.env.solver.t.name(a);
                self.env.solver.t.atom(n)
            }
            ExprKind::Str(parts) => {
                // `#{x}` desugars to `to_string(x)`, so an interpolated value must be
                // Display. Dispatching here is what enforces that, and records the
                // resolution for codegen.
                for p in parts {
                    if let ast::StrPart::Interp(inner) = p {
                        let t = self.expr(module, inner, None);
                        if !self.env.is_error(t) {
                            match dispatch::resolve(self.env, "to_string", None, &[t], None) {
                                Ok(sel) => self.result.set_call(inner.id, sel.resolution),
                                Err(err) => self.dispatch_error(inner.span.clone(), err),
                            }
                        }
                    }
                }
                self.env.solver.t.str()
            }

            ExprKind::Path(p) => self.path(module, e, p),

            ExprKind::Unary { op, rhs } => {
                let t = self.expr(module, rhs, None);
                match op {
                    UnOp::Neg | UnOp::Bnot => t,
                    UnOp::Not => self.env.solver.t.bool(),
                }
            }

            ExprKind::Binary { op, lhs, rhs } => self.binary(module, e, *op, lhs, rhs, expected),

            ExprKind::Tuple(v) => {
                let ts: Vec<TyId> = v.iter().map(|x| self.expr(module, x, None)).collect();
                self.env.solver.t.tuple(ts)
            }

            ExprKind::List(elems) => {
                // Push the expected element type down. Without this a nested literal
                // infers its own type from its own elements — `[:ok, [:ok, :ok]]` against
                // `mu type A = :ok | List[A]` made the inner list a `List[:ok]` with
                // 8-byte slots where the outer expected 16-byte `A` slots, and the
                // coercion that could not bridge them quietly zeroed the element.
                let want_elem = expected.and_then(|t| self.element_type(t));
                let mut elem_tys = Vec::new();
                for el in elems {
                    match el {
                        ast::Elem::Value(x) => elem_tys.push(self.expr(module, x, want_elem)),
                        ast::Elem::Spread(x) => {
                            self.expr(module, x, None);
                        }
                    }
                }
                // With an expected type, that is the list's type. Without one — a bare
                // literal, e.g. a `for` iterable — infer `List[T]` where `T` is the union
                // of the elements' types (`never` for the empty list).
                match expected {
                    Some(t) => t,
                    None => {
                        let elem = if elem_tys.is_empty() {
                            self.env.solver.t.never()
                        } else {
                            self.env.solver.t.union_all(&elem_tys)
                        };
                        let name = self.env.solver.t.name("List");
                        self.env.solver.t.nominal(name, vec![elem], vec![])
                    }
                }
            }

            ExprKind::If { cond, then, else_ } => self.if_expr(module, e, cond, then, else_, expected),

            ExprKind::Match { scrutinee, arms } => self.match_expr(module, e, scrutinee, arms, expected),

            ExprKind::Block(b) => self.block(module, b, expected),

            ExprKind::Is { lhs, ty } => {
                self.expr(module, lhs, None);
                let scope = self.type_scope(module);
                self.env.resolve(&scope, ty);
                self.env.solver.t.bool()
            }

            ExprKind::As { lhs, ty } => {
                let from = self.expr(module, lhs, None);
                let scope = self.type_scope(module);
                let to = self.env.resolve(&scope, ty);
                // A cast narrows -- it cannot reach a type the value could never be --
                // except across a newtype boundary, where it wraps or unwraps: a
                // `newtype Meter = f64` is disjoint from `f64`, yet `m as f64` and
                // `x as Meter` are exactly what a newtype is for. So the cast is also
                // valid when one side is a newtype whose representation meets the other.
                let meet = self.env.solver.t.intersect(from, to);
                let ok = !self.env.solver.is_empty(meet)
                    || self.newtype_bridges(from, to)
                    || self.newtype_bridges(to, from);
                if !self.env.is_error(from) && !ok {
                    let (f, t) = (self.show(from), self.show(to));
                    self.error(e.span.clone(), TypeErrorKind::ImpossibleCast { from: f, to: t });
                    return self.poison();
                }
                to
            }

            ExprKind::Return(v) => {
                let want = self.ret;
                let t = match v {
                    Some(x) => self.expr(module, x, want),
                    None => self.env.solver.t.tuple(vec![]),
                };
                // Inside a lambda, `return` returns from the *lambda* -- that is what
                // lowering does, since a lambda is lifted to its own function -- so its
                // type joins the lambda's, not the enclosing function's. Checking it
                // against the enclosing function was unsound: a `str` returned through an
                // `i64` slot compiled clean and was reinterpreted.
                if let Some(frame) = self.lambda_returns.last_mut() {
                    frame.push(t);
                }
                self.env.solver.t.never()
            }

            ExprKind::Throw(x) => {
                let t = self.expr(module, x, None);
                self.note_throw(x.span.clone(), t, false);
                self.env.solver.t.never()
            }

            ExprKind::Break(v) => {
                // A bare `break` exits with no value, which reads as `null`: a loop
                // that can break bare yields `T | null`, and one that only breaks bare
                // yields `null`.
                let t = match v {
                    Some(x) => self.expr(module, x, None),
                    None => self.env.solver.t.null(),
                };
                match self.loop_breaks.last_mut() {
                    Some(breaks) => breaks.push(t),
                    // No enclosing loop -- either there is genuinely none, or a lambda
                    // sits between here and it, which is the same thing at run time.
                    None => self.error(e.span.clone(), TypeErrorKind::OutsideLoop("break".into())),
                }
                self.env.solver.t.never()
            }
            ExprKind::Continue => {
                if self.loop_breaks.is_empty() {
                    self.error(e.span.clone(), TypeErrorKind::OutsideLoop("continue".into()));
                }
                self.env.solver.t.never()
            }

            ExprKind::While { cond, body } => {
                let b = self.env.solver.t.bool();
                self.expr(module, cond, Some(b));
                self.loop_breaks.push(vec![]);
                self.block(module, body, None);
                self.loop_breaks.pop();
                self.env.solver.t.tuple(vec![])
            }
            ExprKind::Loop { body } => {
                self.loop_breaks.push(vec![]);
                self.block(module, body, None);
                let breaks = self.loop_breaks.pop().unwrap_or_default();
                if breaks.is_empty() {
                    // No `break` with a value: the loop either never ends or only
                    // breaks bare, so it yields nothing.
                    self.env.solver.t.never()
                } else {
                    self.env.solver.t.union_all(&breaks)
                }
            }
            ExprKind::For { pat, iter, body } => {
                let t = self.expr(module, iter, None);
                let elem = match self.element_type(t) {
                    Some(e) => e,
                    None => {
                        if !self.env.is_error(t) {
                            let on = self.show(t);
                            self.error(iter.span.clone(), TypeErrorKind::NotIterable(on));
                        }
                        self.poison()
                    }
                };
                self.locals.push(vec![]);
                self.bind_pattern(module, pat, elem);
                self.loop_breaks.push(vec![]);
                self.block(module, body, None);
                self.loop_breaks.pop();
                self.locals.pop();
                self.env.solver.t.tuple(vec![])
            }

            ExprKind::Assert { args, .. } => {
                for a in args {
                    self.expr(module, a, None);
                }
                self.env.solver.t.tuple(vec![])
            }

            ExprKind::Call { callee, generics, args } => {
                self.call(module, e, callee, generics, args, expected)
            }

            ExprKind::Field { base, name } => {
                let t = self.expr(module, base, None);
                self.check_opacity(module, e.span.clone(), t, name);
                let label = self.env.solver.t.name(name);
                let p = narrow::project_field(&mut self.env.solver, t, label);
                self.projected(e.span.clone(), p, name, t)
            }

            ExprKind::Error => self.poison(),

            ExprKind::Lambda { params, body } => self.lambda(module, e, params, body, expected),

            // Not yet: each needs something that does not exist. A guess here is
            // exactly the fallback this design has no room for.
            ExprKind::RecordLit { path, fields, spread } => {
                self.record_lit(module, e, path, fields, spread, expected)
            }

            ExprKind::Index { base, index } => {
                let t = self.expr(module, base, None);
                // A two-argument collection -- `Map[K, V]` -- is keyed by K (#0) and
                // yields V (#1). A one-argument `List[T]` is keyed by i64 and yields T.
                let arg1 = self.arg_type(t, 1);
                let (key, value) = match arg1 {
                    Some(v) => (self.arg_type(t, 0), Some(v)),
                    None => (Some(self.env.solver.t.i64()), self.element_type(t)),
                };
                if let Some(k) = key {
                    self.expr(module, index, Some(k));
                } else {
                    self.expr(module, index, None);
                }
                match value {
                    Some(v) => v,
                    None => {
                        if !self.env.is_error(t) {
                            let on = self.show(t);
                            self.error(e.span.clone(), TypeErrorKind::NotIndexable(on));
                        }
                        self.poison()
                    }
                }
            }

            ExprKind::Try { form, body, catch } => {
                self.try_expr(module, e.id, *form, body, catch, expected)
            }
        }
    }

    /// A lambda, in checking mode. Its parameter types come from their annotations,
    /// or from the expected arrow flowing in — `map(xs, (x) => x + 1)` gets `x: i64`
    /// from `map`'s parameter. A parameter with neither is an error, not a guess:
    /// inferring it from a later use, or from the body, is unification, which this
    /// bidirectional checker does not do. See `decisions.md` on Castagna.
    fn lambda(
        &mut self,
        module: &[String],
        e: &Expr,
        params: &[ast::LambdaParam],
        body: &Expr,
        expected: Option<TyId>,
    ) -> TyId {
        let scope = self.type_scope(module);
        let want = expected.and_then(|t| self.env.solver.t.as_arrow(t));

        // Everything already on the stack is captured; the lambda's own scope starts
        // here. An assignment to a name below this floor is a rebind of a capture.
        self.capture_floors.push(self.locals.len());
        self.locals.push(vec![]);
        let mut param_tys = Vec::with_capacity(params.len());
        for (i, p) in params.iter().enumerate() {
            let t = match (&p.ty, want.as_ref().and_then(|a| a.params.get(i))) {
                (Some(spec), _) => self.env.resolve(&scope, spec),
                (None, Some(&pt)) => pt,
                (None, None) => {
                    self.error(e.span.clone(), TypeErrorKind::LambdaParamNeedsType(p.name.clone()));
                    self.poison()
                }
            };
            // A lambda param carries no span of its own; the lambda's is close enough
            // for a diagnostic, and a param is never a capture anyway.
            self.bind(&p.name, t, e.span.clone());
            param_tys.push(t);
        }

        // A lambda body is a new *function* context, and every function-scoped thing has
        // to be reset for it -- not just `throws`. Leaving the rest in place let control
        // flow escape a boundary it cannot actually cross at run time, because the lambda
        // is lifted into its own function:
        //
        //   `return`      was checked against the enclosing function's return type, so a
        //                 `str` could be returned through an `i64` slot. Unsound.
        //   `throw`       was absorbed by an enclosing `try`, so the checker called an
        //                 error handled that escapes uncaught at run time.
        //   `break`       resolved to an enclosing loop, and reached `unreachable`.
        //
        // A lambda cannot *declare* `throws` — there is no syntax — but it never needed
        // to: parameters are inputs and need a source, while the return type and throws
        // are outputs, always derivable from the body. So the body is checked in `Infer`
        // mode and whatever propagates out is collected.
        let want_ret = want.as_ref().map(|a| a.ret);
        let never = self.env.solver.t.never();
        let saved_throws = self.throws.replace(Throws::Infer);
        let saved_ret = self.ret.take();
        let saved_sinks = std::mem::take(&mut self.throw_sinks);
        let saved_breaks = std::mem::take(&mut self.loop_breaks);
        self.ret = want_ret;
        self.lambda_returns.push(vec![]);
        self.lambda_throws.push(vec![]);

        let tail = self.expr(module, body, want_ret);

        let returned = self.lambda_returns.pop().unwrap_or_default();
        let thrown = self.lambda_throws.pop().unwrap_or_default();
        self.throws = saved_throws;
        self.ret = saved_ret;
        self.throw_sinks = saved_sinks;
        self.loop_breaks = saved_breaks;
        self.locals.pop();
        self.capture_floors.pop();

        // The lambda's return type is its tail unioned with whatever its `return`s give.
        let ret = returned.into_iter().fold(tail, |acc, t| self.union_branches(acc, t));

        // Its `throws` is what the body propagates — plus the expected arrow's clause,
        // when one flows in. Adopting the clause matters beyond subtyping: the clause is
        // part of the calling convention (a throwing closure returns a tagged result), so
        // a lambda filling a `(i64) throws E -> i64` slot must *be* one, even when its
        // own body cannot fail. Widening the throws is free at creation and the body's
        // errors still have to fit the clause, checked by `expr`'s assignability.
        let mut throws = thrown.into_iter().fold(never, |acc, t| self.union_branches(acc, t));
        if let Some(a) = &want {
            throws = self.union_branches(throws, a.throws);
        }

        let arrow = self.env.solver.t.arrow(param_tys, throws, ret);
        self.result.set_lambda(e.id, arrow);
        arrow
    }

    fn record_lit(
        &mut self,
        module: &[String],
        e: &Expr,
        path: &Option<Vec<String>>,
        fields: &[ast::FieldInit],
        spread: &Option<Box<Expr>>,
        expected: Option<TyId>,
    ) -> TyId {
        // A named literal builds a nominal record. Resolve the type, then check its
        // fields exactly as an anonymous literal is checked against a target: every
        // field declared, right types, no extras. Generic records need their
        // arguments inferred from the fields, which is not built yet -- those still
        // flow the expected type unchecked.
        if let Some(p) = path {
            // Building one — with or without a spread, which is an update and so equally
            // a way to set a field the module means to control.
            let what = if spread.is_some() { "it can be updated" } else { "it can be built" };
            self.check_opaque_path(module, e.span.clone(), p, what);
            let key = self.env.lookup(module, p);
            if let Some(key) = &key {
                if self.env.is_generic(key) {
                    return self.generic_record_lit(module, e, key, fields, spread);
                }
                let scope = self.type_scope(module);
                let spec = ast::TypeSpec {
                    kind: ast::TypeSpecKind::Named { path: p.clone(), args: vec![] },
                    span: e.span.clone(),
                };
                let record_ty = self.env.resolve(&scope, &spec);
                if let Some(target_fields) = self.record_fields(record_ty) {
                    self.check_record_fields(module, e, fields, spread, &target_fields);
                    return record_ty;
                }
            }
            for f in fields {
                self.expr(module, &f.value, None);
            }
            if let Some(s) = spread {
                self.expr(module, s, None);
            }
            return expected.unwrap_or_else(|| self.poison());
        }

        // An anonymous record. A fresh literal is checked exactly against the type
        // it is written for: excess fields the target does not declare are an error
        // (a typo, not a widening), while a missing nullable field is fine. A record
        // held in a variable still flows by width subtyping -- this excess check is
        // TypeScript's, and it is why a literal differs from a value here.
        if let Some(target_fields) = expected.and_then(|exp| self.record_fields(exp)) {
            self.check_record_fields(module, e, fields, spread, &target_fields);
            return expected.expect("target present");
        }

        let mut seen: Vec<String> = Vec::new();
        let mut field_tys: Vec<(super::types::NameId, TyId)> = Vec::new();
        for f in fields {
            if seen.contains(&f.name) {
                self.error(f.span.clone(), TypeErrorKind::DuplicateField(f.name.clone()));
            }
            seen.push(f.name.clone());
            let t = self.expr(module, &f.value, None);
            let label = self.env.solver.t.name(&f.name);
            field_tys.push((label, t));
        }
        if let Some(s) = spread {
            self.expr(module, s, None);
        }
        self.env.solver.t.struct_ty(field_tys)
    }

    /// Check a literal's fields against a record's declared fields: each present and
    /// declared (no extras), each typed, and no required (non-nullable) field missing.
    fn check_record_fields(
        &mut self,
        module: &[String],
        e: &Expr,
        fields: &[ast::FieldInit],
        spread: &Option<Box<Expr>>,
        target: &[(String, TyId)],
    ) {
        let mut seen: Vec<String> = Vec::new();
        for f in fields {
            if seen.contains(&f.name) {
                self.error(f.span.clone(), TypeErrorKind::DuplicateField(f.name.clone()));
            }
            seen.push(f.name.clone());
            match target.iter().find(|(n, _)| *n == f.name) {
                Some((_, want)) => {
                    // Like `expr`, but a mismatch names the field rather than reporting a
                    // bare type pair -- `{ timeout: "x" }` should point at `timeout`.
                    let want = *want;
                    let got = self.infer(module, &f.value, Some(want));
                    self.result.set_ty(f.value.id, got);
                    if !self.assignable(got, want) {
                        let (expected, found) = (self.show(want), self.show(got));
                        self.error(
                            f.value.span.clone(),
                            TypeErrorKind::FieldTypeMismatch { field: f.name.clone(), expected, found },
                        );
                    }
                }
                None => {
                    self.expr(module, &f.value, None);
                    let on = self.record_name(target);
                    self.error(f.span.clone(), TypeErrorKind::NoField { field: f.name.clone(), on });
                }
            }
        }
        if let Some(s) = spread {
            self.expr(module, s, None);
            return;
        }
        for (name, fty) in target {
            if !seen.contains(name) && !self.is_nullable(*fty) {
                self.error(e.span.clone(), TypeErrorKind::MissingField(name.clone()));
            }
        }
    }

    /// A named literal for a generic record: `Box { item: 1 }`. Instantiate the
    /// record with fresh rigid variables, infer them from the field values, then
    /// substitute -- so the literal's type is `Box[i64]`, not `Box[T]`.
    fn generic_record_lit(
        &mut self,
        module: &[String],
        e: &Expr,
        key: &str,
        fields: &[ast::FieldInit],
        spread: &Option<Box<Expr>>,
    ) -> TyId {
        use std::collections::{HashMap, HashSet};
        let names = self.env.generic_names(key);
        let var_names: HashSet<_> = names.iter().map(|n| self.env.solver.t.name(n)).collect();
        let var_args: Vec<TyId> = names
            .iter()
            .map(|n| {
                let nn = self.env.solver.t.name(n);
                self.env.solver.t.var(nn)
            })
            .collect();
        let templated = self.env.instantiate(key, var_args, &e.span);
        let tfields = self.record_fields(templated).unwrap_or_default();

        // Infer the variables from the fields, remembering each field's own type.
        let mut subst: HashMap<_, TyId> = HashMap::new();
        let mut given: Vec<(String, TyId)> = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        for f in fields {
            if seen.contains(&f.name) {
                self.error(f.span.clone(), TypeErrorKind::DuplicateField(f.name.clone()));
            }
            seen.push(f.name.clone());
            let ft = self.expr(module, &f.value, None);
            match tfields.iter().find(|(n, _)| *n == f.name) {
                Some((_, tmpl)) => {
                    super::generic::infer(&mut self.env.solver.t, *tmpl, ft, &var_names, &mut subst);
                    given.push((f.name.clone(), ft));
                }
                None => {
                    let on = key.rsplit("::").next().unwrap_or(key).to_string();
                    self.error(f.span.clone(), TypeErrorKind::NoField { field: f.name.clone(), on });
                }
            }
        }
        if let Some(s) = spread {
            self.expr(module, s, None);
        }

        // Now check each field against the resolved parameter type -- this catches a
        // variable pinned by one field and violated by another, e.g. Pair[T] with
        // mismatched a and b.
        for (name, got) in &given {
            if let Some((_, tmpl)) = tfields.iter().find(|(n, _)| n == name) {
                let want = self.env.solver.t.substitute(*tmpl, &subst);
                if !self.assignable(*got, want) {
                    let (g, w) = (self.show(*got), self.show(want));
                    self.error(e.span.clone(), TypeErrorKind::Mismatch { expected: w, found: g });
                }
            }
        }
        if spread.is_none() {
            for (name, tmpl) in &tfields {
                if !seen.contains(name) {
                    let concrete = self.env.solver.t.substitute(*tmpl, &subst);
                    if !self.is_nullable(concrete) {
                        self.error(e.span.clone(), TypeErrorKind::MissingField(name.clone()));
                    }
                }
            }
        }
        self.env.solver.t.substitute(templated, &subst)
    }

    fn record_name(&mut self, target: &[(String, TyId)]) -> String {
        let fs: Vec<String> = target.iter().map(|(n, _)| n.clone()).collect();
        format!("{{{}}}", fs.join(", "))
    }

    fn is_nullable(&self, ty: TyId) -> bool {
        self.env.solver.t.data(ty).base & super::types::B_NULL != 0
    }

    /// A type that is only atoms -- `:ok`, `:ok | :err`. All atoms share one
    /// comparison domain, so two of them may be compared for equality.
    fn is_atomic(&self, ty: TyId) -> bool {
        let d = self.env.solver.t.data(ty);
        d.base == 0
            && self.env.solver.t.atomset_of(d.vars).is_empty_set()
            && !self.env.solver.t.atomset_of(d.atoms).is_empty_set()
            && d.records == super::bdd::FALSE
            && d.tuples == super::bdd::FALSE
            && d.arrows == super::bdd::FALSE
    }

    /// A type with a structural order, given whatever `where T: Ord` bounds are in scope.
    ///
    /// The rule itself lives in `typecheck::ordered`, because the `marker Ord` bound needs
    /// the same answer when it is discharged at a call site. See that module for why order
    /// is infectious and why the bound set is threaded through the recursion.
    fn is_ordered(&self, ty: TyId) -> bool {
        super::ordered::is_ordered(self.env, ty, &self.ord_bound_vars())
    }

    /// The type parameters this signature declared `where T: Ord` for.
    fn ord_bound_vars(&self) -> std::collections::HashSet<String> {
        self.bounds
            .iter()
            .filter(|(_, p)| {
                let proto = self.env.protocol(*p);
                proto.is_marker && proto.name == "Ord"
            })
            .map(|(n, _)| n.clone())
            .collect()
    }

    /// Record that something throws `throws`. Inside a `try` it lands in the sink to
    /// be caught or propagated; from a call outside any `try` it is a bare throwing
    /// call, a compile error; from a `throw` statement outside a `try` it propagates
    /// to the enclosing function's declared `throws`.
    fn note_throw(&mut self, span: Span, throws: TyId, from_call: bool) {
        let never = self.env.solver.t.never();
        if self.env.is_error(throws) || throws == never {
            return;
        }
        if let Some(sink) = self.throw_sinks.last_mut() {
            sink.push(throws);
        } else if from_call {
            self.error(span, TypeErrorKind::BareThrowingCall);
        } else {
            match self.throws {
                // Escaping `main`: it must be reportable, so it must implement `Error`.
                // Resolving `message` for it is the check — and it answers for a union by
                // requiring every variant to have an impl.
                Some(Throws::ImplicitError) => {
                    if !self.implements_error(throws) {
                        let t = self.show(throws);
                        self.error(span, TypeErrorKind::NotAnError { thrown: t });
                    }
                }
                // A lambda body: nothing to check against — the escape *becomes* part
                // of the lambda's inferred `throws`.
                Some(Throws::Infer) => {
                    if let Some(frame) = self.lambda_throws.last_mut() {
                        frame.push(throws);
                    }
                }
                other => {
                    let want = match other {
                        Some(Throws::Declared(t)) => t,
                        _ => never,
                    };
                    if !self.assignable(throws, want) {
                        let (t, w) = (self.show(throws), self.show(want));
                        self.error(span, TypeErrorKind::Throws { thrown: t, declared: w });
                    }
                }
            }
        }
    }

    /// Whether a type implements `Error`. Asking dispatch to resolve `message` for it is
    /// the whole check: it succeeds only when every value the type admits has an impl.
    fn implements_error(&mut self, ty: TyId) -> bool {
        let Some(proto) = self.env.lookup_protocol(&[], &["Error".to_string()]) else {
            return true; // no prelude in scope; nothing to enforce
        };
        dispatch::resolve(self.env, "message", Some(proto), &[ty], None).is_ok()
    }

    fn try_expr(
        &mut self,
        module: &[String],
        id: crate::ast::ExprId,
        form: ast::TryForm,
        body: &Expr,
        catch: &Option<ast::CatchArm>,
        expected: Option<TyId>,
    ) -> TyId {
        self.throw_sinks.push(vec![]);
        // The body's value flows the expected type down only when nothing follows it;
        // with a catch the arms are unioned, so let both synthesize.
        let val = self.expr(module, body, if catch.is_some() { None } else { expected });
        let thrown = self.throw_sinks.pop().unwrap_or_default();
        let caught = self.env.solver.t.union_all(&thrown);
        let never = self.env.solver.t.never();

        // Hand the exact error type to lowering, so the handler's parameter is concrete
        // rather than erased.
        let handled = if thrown.is_empty() { never } else { caught };
        self.result.set_caught(id, handled);

        if let Some(arm) = catch {
            // The error union is handled here, not propagated. `catch` binds it.
            self.locals.push(vec![]);
            let bound = if thrown.is_empty() { never } else { caught };
            self.bind(&arm.binding, bound, arm.span.clone());
            let handled = self.block(module, &arm.body, expected);
            self.locals.pop();
            return self.union_branches(val, handled);
        }

        match form {
            // Propagate: the errors become the enclosing function's to declare.
            ast::TryForm::Propagate => {
                self.note_throw(body.span.clone(), caught, false);
                val
            }
            // Soften: a failure yields null instead.
            ast::TryForm::Soften => {
                let null = self.env.solver.t.null();
                self.env.solver.t.union(val, null)
            }
            // Assert: a failure panics, so the value is the success type unchanged.
            ast::TryForm::Assert => val,
        }
    }

    /// The element of a single-argument collection -- `List[T]` carries it in `#0`.
    /// `for x in xs` and `xs[i]` both read it.
    fn element_type(&mut self, ty: TyId) -> Option<TyId> {
        self.arg_type(ty, 0)
    }

    /// A generic argument by position: `#0`, `#1`. `None` when the type has no such
    /// slot, which is how a `Map` (two arguments) is told from a `List` (one).
    fn arg_type(&mut self, ty: TyId, i: usize) -> Option<TyId> {
        let label = self.env.solver.t.arg_label(i);
        narrow::project_field(&mut self.env.solver, ty, label).ty()
    }

    /// The nominal name of a type, when it is exactly one named record atom. This is
    /// the same question `ir::repr::nominal_name` asks of a lowered type, asked here of
    /// a checked one.
    fn nominal_of(&self, ty: TyId) -> Option<String> {
        let t = &self.env.solver.t;
        let d = t.data(ty);
        match t.rec_bdd.paths(d.records).as_slice() {
            [(pos, neg)] if neg.is_empty() && pos.len() == 1 => {
                let tag = t.rec_atoms[pos[0] as usize].get(t.nominal_label);
                let atoms = t.atomset_of(t.data(tag).atoms);
                (!atoms.neg && atoms.names.len() == 1)
                    .then(|| t.name_str(atoms.names[0]).to_string())
            }
            _ => None,
        }
    }

    /// Reject reaching inside an `opaque` record from outside the module that owns it.
    ///
    /// The three ways in are reading a field, building a literal, and destructuring a
    /// pattern; all of them land here. Holding and passing a value are deliberately not
    /// among them — opacity hides the contents, not the type.
    ///
    /// Visible in the declaring module and its immediate parent, which is what
    /// `RecordDecl::opaque` has always documented and what `std::fs`'s `internal mod raw`
    /// depends on: the inner module declares the handle, the outer module implements the
    /// API over it.
    ///
    /// Until this landed, `opaque` was parsed, stored, and read only by the formatter —
    /// so `std::fs`'s claim that "nothing outside can read a descriptor out of it" was
    /// not true, merely unexercised, because `File` happens to have no fields.
    fn check_opaque_name(&mut self, module: &[String], span: Span, name: &str, what: &str) {
        let Some(owner) = self.env.opaque_record_named(module, name) else { return };
        let owner = owner.to_vec();
        self.report_opacity(module, span, name, what, owner);
    }

    /// The same rule for a record the source *names*, where the written path resolves
    /// unambiguously and no fallback is needed.
    fn check_opaque_path(&mut self, module: &[String], span: Span, path: &[String], what: &str) {
        let Some(owner) = self.env.opaque_record_at(module, path) else { return };
        let owner = owner.to_vec();
        let name = path.last().cloned().unwrap_or_default();
        self.report_opacity(module, span, &name, what, owner);
    }

    fn report_opacity(
        &mut self,
        module: &[String],
        span: Span,
        name: &str,
        what: &str,
        owner: Vec<String>,
    ) {
        let visible = module == owner.as_slice()
            || (owner.len() == module.len() + 1 && owner[..module.len()] == *module);
        if visible {
            return;
        }
        let shown = if owner.is_empty() { "the prelude".to_string() } else { owner.join("::") };
        self.error(
            span,
            TypeErrorKind::OpaqueRecord {
                record: name.to_string(),
                module: shown,
                what: what.to_string(),
            },
        );
    }

    /// The field-read entry point: the record is whatever the base expression turned out
    /// to be, so the name has to come from the type rather than from syntax.
    fn check_opacity(&mut self, module: &[String], span: Span, ty: TyId, field: &str) {
        let Some(name) = self.nominal_of(ty) else { return };
        self.check_opaque_name(module, span, &name, &format!("its field `{field}` is readable"));
    }

    /// The declared fields of a record type -- the user-written ones, dropping the
    /// reserved `#nominal` and `#0`, `#1` generic-argument slots. `None` when `ty`
    /// is not a single record atom.
    fn record_fields(&self, ty: TyId) -> Option<Vec<(String, TyId)>> {
        let t = &self.env.solver.t;
        let d = t.data(ty);
        match t.rec_bdd.paths(d.records).as_slice() {
            [(pos, neg)] if neg.is_empty() && pos.len() == 1 => {
                let atom = &t.rec_atoms[pos[0] as usize];
                Some(
                    atom.fields
                        .iter()
                        .map(|&(l, ft)| (t.name_str(l).to_string(), ft))
                        .filter(|(n, _)| !n.starts_with('#'))
                        .collect(),
                )
            }
            _ => None,
        }
    }

    fn path(&mut self, module: &[String], e: &Expr, p: &[String]) -> TyId {
        if let [one] = p {
            if let Some(t) = self.lookup(one) {
                return t;
            }
        }
        let joined = p.join("::");
        if let Some(sig) = self.env.fn_named(module, p) {
            // A function used as a value, throwing or not: its arrow carries the
            // `throws`, the closure repr carries it in turn, and the adapter thunk
            // returns the tagged result — the calling convention survives the trip.
            return sig.ty;
        }
        // A name that exists but is fenced off reports why, rather than "not in scope".
        if let Some(owner) = self.env.hidden_by_internal(module, p) {
            self.error(e.span.clone(), TypeErrorKind::Internal { name: joined, owner });
            return self.poison();
        }
        self.error(e.span.clone(), TypeErrorKind::UnknownName(joined));
        self.poison()
    }

    fn binary(&mut self, module: &[String], e: &Expr, op: BinOp, lhs: &Expr, rhs: &Expr, expected: Option<TyId>) -> TyId {
        match op {
            BinOp::And | BinOp::Or => {
                let b = self.env.solver.t.bool();
                self.expr(module, lhs, Some(b));
                self.expr(module, rhs, Some(b));
                b
            }
            // Equality is total: any two values may be compared, disjoint ones are
            // simply not equal. `:ok == :err` is false, not an error, and that is
            // what makes an atom union behave like a sum type.
            // Equality needs comparable operands: they overlap, or both are atoms
            // (which form one comparison domain). `:ok == :err` is false, but
            // `:ok == "ok"` compares an atom to a string, which is a mistake.
            BinOp::Eq | BinOp::Ne => {
                let l = self.expr(module, lhs, None);
                let r = self.expr(module, rhs, None);
                let is_null = |e: &Expr| matches!(e.kind, ExprKind::Null);
                if !is_null(lhs) && !is_null(rhs) && !self.env.is_error(l) && !self.env.is_error(r) {
                    let meet = self.env.solver.t.intersect(l, r);
                    let both_atoms = self.is_atomic(l) && self.is_atomic(r);
                    // One side being a subtype of the other is comparable even when the
                    // meet is empty: `xs == []` compares `List[i64]` with `List[never]`,
                    // and `List[never]` has no *inhabitants* to intersect with, so the
                    // overlap test alone rejected the natural way to ask "is this empty".
                    let related = self.env.solver.is_subtype(l, r) || self.env.solver.is_subtype(r, l);
                    if self.env.solver.is_empty(meet) && !both_atoms && !related {
                        let (a, b) = (self.show(l), self.show(r));
                        self.error(e.span.clone(), TypeErrorKind::Incomparable { left: a, right: b });
                    } else if !super::ordered::is_equatable(self.env, l)
                        || !super::ordered::is_equatable(self.env, r)
                    {
                        // Both sides, not the meet: `Map[str, i64] == Map[str, i64]` meets
                        // itself, and it is the operands the backend has to compare.
                        let ty = self.show(l);
                        self.error(e.span.clone(), TypeErrorKind::Unequatable { ty });
                    }
                }
                self.env.solver.t.bool()
            }
            // Ordering needs an order. `1 < 2`, `"a" < "b"` and two of the same record are
            // fine; `1 < "s"` has no common type, and a union, an atom or a function has
            // no order even when both sides have the same type.
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let l = self.expr(module, lhs, None);
                let r = self.expr(module, rhs, None);
                let meet = self.env.solver.t.intersect(l, r);
                if !self.env.is_error(l) && !self.env.is_error(r) {
                    if self.env.solver.is_empty(meet) {
                        let (a, b) = (self.show(l), self.show(r));
                        self.error(e.span.clone(), TypeErrorKind::Incomparable { left: a, right: b });
                    } else if !self.is_ordered(meet) {
                        let shown = self.show(meet);
                        self.error(e.span.clone(), TypeErrorKind::Unordered { ty: shown });
                    }
                }
                self.env.solver.t.bool()
            }
            BinOp::Orelse => {
                let l = self.expr(module, lhs, None);
                let r = self.expr(module, rhs, None);
                // `orelse` replaces the null arm, so the result is the rest of the
                // left plus the right.
                let null = self.env.solver.t.null();
                let non_null = self.env.solver.t.diff(l, null);
                self.env.solver.t.union(non_null, r)
            }
            BinOp::Pipe => {
                // `a |> f(b)` is `f(a, b)`: the receiver becomes the first argument.
                if let ExprKind::Call { callee, generics, args } = &rhs.kind {
                    let mut piped = Vec::with_capacity(args.len() + 1);
                    piped.push(lhs.clone());
                    piped.extend(args.iter().cloned());
                    return self.call(module, rhs, callee, generics, &piped, expected);
                }
                // `a |> f` with a bare callee applies it to the receiver.
                let f = self.expr(module, rhs, None);
                self.apply(module, e, "the right of `|>`", f, std::slice::from_ref(lhs))
            }
            _ => {
                let l = self.expr(module, lhs, None);
                let r = self.expr(module, rhs, None);
                if !self.assignable(r, l) && !self.assignable(l, r) {
                    let (a, b) = (self.show(l), self.show(r));
                    self.error(e.span.clone(), TypeErrorKind::Mismatch { expected: a, found: b });
                    return self.poison();
                }
                l
            }
        }
    }

    fn if_expr(
        &mut self,
        module: &[String],
        e: &Expr,
        cond: &Expr,
        then: &ast::Block,
        else_: &Option<Box<Expr>>,
        expected: Option<TyId>,
    ) -> TyId {
        let b = self.env.solver.t.bool();
        self.expr(module, cond, Some(b));

        let Some(other) = else_ else {
            self.block(module, then, None);
            // With no `else`, the `if` yields `()` when the condition is false. That is
            // fine wherever `()` is accepted -- a statement, or a `-> null`/`-> ()` tail
            // -- and an error only where a real value is required.
            let unit = self.env.solver.t.tuple(vec![]);
            let rejects_unit = expected.is_some_and(|exp| !self.env.solver.is_subtype(unit, exp));
            if rejects_unit {
                self.error(e.span.clone(), TypeErrorKind::IfWithoutElse);
                return self.poison();
            }
            return unit;
        };

        let a = self.block(module, then, expected);
        let c = self.expr(module, other, expected);
        self.union_branches(a, c)
    }

    fn match_expr(
        &mut self,
        module: &[String],
        e: &Expr,
        scrutinee: &Expr,
        arms: &[ast::MatchArm],
        expected: Option<TyId>,
    ) -> TyId {
        let subject = self.expr(module, scrutinee, None);
        // A bare-variable scrutinee is re-narrowed inside each arm, so `is Circle`
        // makes `s.r` legal: the arm sees `s` as the member, not the whole union.
        let scrut_var = self.scrutinee_var(scrutinee);
        let mut result = self.env.solver.t.never();

        // The running residual: what values could still reach this arm, given the
        // arms above it already peeled off theirs. Narrowing against it — not the
        // full subject — is why a bare binding after `is null` receives the non-null
        // half, and why exhaustiveness falls out as `remaining` reaching empty.
        let mut remaining = subject;
        // `bool` is one base bit, not `:true | :false`, so a boolean literal types
        // as the whole `bool` and cannot be subtracted precisely. Track the two
        // values by hand: seeing both, unguarded, exhausts `bool`.
        let (mut saw_true, mut saw_false) = (false, false);
        for arm in arms {
            let test = self.arm_test(module, arm, subject);
            self.locals.push(vec![]);
            let bound = match test {
                Some(t) => self.env.solver.t.intersect(remaining, t.ty),
                None => remaining,
            };
            if let Some(v) = &scrut_var {
                self.bind(v, bound, scrutinee.span.clone());
            }
            self.bind_pattern(module, &arm.pat, bound);
            if let Some(g) = &arm.guard {
                let b = self.env.solver.t.bool();
                self.expr(module, g, Some(b));
            }
            let t = self.expr(module, &arm.body, expected);
            self.locals.pop();
            result = self.union_branches(result, t);

            // Only an exact, unguarded arm removes anything from the fallthrough: `1`
            // is one i64 among many, and a guard can always reject.
            if let Some(test) = test {
                let mut test = test;
                if arm.guard.is_some() {
                    test = test.guarded();
                }
                let covered = test.covered(&mut self.env.solver);
                remaining = self.env.solver.t.diff(remaining, covered);
            }
            if let ast::PatternKind::Literal(lit) = &arm.pat.kind {
                if let (ExprKind::Bool(b), None) = (&lit.kind, &arm.guard) {
                    if *b { saw_true = true } else { saw_false = true }
                }
            }
        }
        if saw_true && saw_false {
            let bool_ty = self.env.solver.t.bool();
            remaining = self.env.solver.t.diff(remaining, bool_ty);
        }

        // Exhaustiveness falls out: whatever is left in `remaining` names exactly the
        // values no arm matched.
        if !self.env.solver.is_empty(remaining) && !self.env.is_error(subject) {
            let missing = self.show(remaining);
            self.error(e.span.clone(), TypeErrorKind::NotExhaustive { missing });
        }
        result
    }

    /// The scrutinee's variable name when it is a bare local, so match arms can
    /// re-narrow it in place. A field access or call has no name to rebind.
    fn scrutinee_var(&self, scrutinee: &Expr) -> Option<String> {
        match &scrutinee.kind {
            ExprKind::Path(segs) => match segs.as_slice() {
                [one] if self.lookup(one).is_some() => Some(one.clone()),
                _ => None,
            },
            _ => None,
        }
    }

    /// What an arm tests for, or `None` when it is a plain binding (which admits
    /// everything, and so is not a test at all).
    fn arm_test(&mut self, module: &[String], arm: &ast::MatchArm, subject: TyId) -> Option<narrow::Test> {
        let scope = self.type_scope(module);
        match &arm.pat.kind {
            ast::PatternKind::Wildcard | ast::PatternKind::Bind(_) => {
                Some(narrow::Test::exact(subject))
            }
            ast::PatternKind::Is(spec) => {
                let t = self.env.resolve(&scope, spec);
                Some(narrow::Test::exact(t))
            }
            ast::PatternKind::Literal(lit) => {
                let t = self.expr(module, lit, None);
                // An atom and `null` are singletons; an integer literal is one i64
                // among many, so it covers nothing.
                Some(match &lit.kind {
                    ExprKind::Atom(_) | ExprKind::Null => narrow::Test::exact(t),
                    _ => narrow::Test::inexact(t),
                })
            }
            // A named record pattern selects its member and narrows to it, so
            // `Circle { r }` reads `r` as an `i64`. It covers that member only when
            // every field pattern is irrefutable -- `Circle { r: 0 }` matches one
            // Circle among many, so it covers nothing and needs a fallthrough.
            ast::PatternKind::Record { path: Some(p), fields, .. } => {
                let spec = ast::TypeSpec {
                    kind: ast::TypeSpecKind::Named { path: p.clone(), args: vec![] },
                    span: arm.pat.span.clone(),
                };
                let t = self.env.resolve(&scope, &spec);
                let exact = fields.iter().all(Self::field_irrefutable);
                Some(if exact { narrow::Test::exact(t) } else { narrow::Test::inexact(t) })
            }
            _ => None,
        }
    }

    /// Whether a field pattern can never reject — a shorthand or a bind always
    /// matches; a nested literal or `is` can fail.
    fn field_irrefutable(f: &ast::FieldPat) -> bool {
        f.pat.as_ref().is_none_or(Self::pat_irrefutable)
    }

    fn pat_irrefutable(p: &ast::Pattern) -> bool {
        match &p.kind {
            ast::PatternKind::Bind(_) | ast::PatternKind::Wildcard => true,
            ast::PatternKind::Record { fields, .. } => fields.iter().all(Self::field_irrefutable),
            ast::PatternKind::Tuple(ps) => ps.iter().all(Self::pat_irrefutable),
            _ => false,
        }
    }

    fn call(
        &mut self,
        module: &[String],
        e: &Expr,
        callee: &Expr,
        generics: &[ast::TypeSpec],
        args: &[Expr],
        expected: Option<TyId>,
    ) -> TyId {
        // `x.f(..)` is either a call of a field that holds a function, or method-call
        // syntax -- which Neon does not have. Tell them apart by whether `f` is a
        // field: if not, suggest the free-function or pipe form rather than letting
        // it fail as a plain missing field.
        if let ExprKind::Field { base, name } = &callee.kind {
            let base_ty = self.expr(module, base, None);
            let label = self.env.solver.t.name(name);
            let field = narrow::project_field(&mut self.env.solver, base_ty, label);
            if field.ty().is_none() && !self.env.is_error(base_ty) {
                let on = self.show(base_ty);
                self.error(callee.span.clone(), TypeErrorKind::DotCall { method: name.clone(), on });
                return self.poison();
            }
        }

        let ExprKind::Path(p) = &callee.kind else {
            // Any other expression producing a value: a lambda, a field holding a
            // function, a parenthesised call. It is callable iff its type is an arrow.
            let t = self.expr(module, callee, None);
            return self.apply(module, e, "this expression", t, args);
        };

        // Lexical first: a local shadows everything. A local of arrow type is a
        // first-class value being called, not a name to look up in the fn table.
        if let [one] = p.as_slice() {
            if let Some(t) = self.lookup(one) {
                self.result.set_ty(callee.id, t);
                return self.apply(module, e, one, t, args);
            }
        }

        // Then a module fn, which shadows protocols.
        if let Some(sig) = self.env.fn_named(module, p).cloned() {
            self.result.set_ty(callee.id, sig.ty);
            return self.direct_call(module, e, &sig, generics, args, expected);
        }

        // A function that exists but is fenced off says so, rather than falling through
        // to protocol dispatch and reporting a missing method.
        if let Some(owner) = self.env.hidden_by_internal(module, p) {
            self.error(
                callee.span.clone(),
                TypeErrorKind::Internal { name: p.join("::"), owner },
            );
            return self.poison();
        }

        let arg_tys: Vec<TyId> = args
            .iter()
            .map(|a| {
                // An argument is a value position even when the callee is a protocol
                // method whose parameter type is not yet known, so a bare `if` is
                // reported here rather than as the `()` it would otherwise dispatch on.
                self.reject_bare_if(a);
                self.expr(module, a, None)
            })
            .collect();
        let (name, qualified) = match p.split_last() {
            // A bare name may have been imported as a specific protocol's method.
            Some((last, [])) => (last.clone(), self.env.imported_method(module, last)),
            Some((last, rest)) => (last.clone(), self.env.lookup_protocol(module, rest)),
            None => return self.poison(),
        };

        match dispatch::resolve(self.env, &name, qualified, &arg_tys, expected) {
            Ok(s) => {
                if let dispatch::Resolution::Bound { param, protocol } = &s.resolution {
                    let ok = self.bounds.iter().any(|(n, p)| {
                        n == param && self.env.protocol_extends(*p, *protocol)
                    });
                    if !ok {
                        let pname = self.env.protocols()[protocol.0].name.clone();
                        self.error(
                            e.span.clone(),
                            TypeErrorKind::UnsatisfiedBound { ty: param.clone(), protocol: pname },
                        );
                    }
                }
                self.result.set_call(e.id, s.resolution.clone());
                self.note_throw(e.span.clone(), s.throws, true);
                s.ret
            }
            Err(err) => {
                self.dispatch_error(e.span.clone(), err);
                self.poison()
            }
        }
    }

    fn direct_call(
        &mut self,
        module: &[String],
        e: &Expr,
        sig: &super::env::FnSig,
        generics: &[ast::TypeSpec],
        args: &[Expr],
        expected: Option<TyId>,
    ) -> TyId {
        if sig.params.len() != args.len() {
            self.error(
                e.span.clone(),
                TypeErrorKind::Arity {
                    name: sig.name.clone(),
                    expected: sig.params.len(),
                    found: args.len(),
                },
            );
        }

        // A non-generic fn: flow each parameter type into its argument as the
        // expected type, so a lambda argument infers its parameters.
        if sig.generics.is_empty() {
            for (a, (_, want)) in args.iter().zip(&sig.params) {
                self.expr(module, a, Some(*want));
            }
            for a in args.iter().skip(sig.params.len()) {
                self.expr(module, a, None);
            }
            self.note_throw(e.span.clone(), sig.throws, true);
            return sig.ret;
        }

        // A generic fn: solve its type parameters, then check under the solution.
        let subst = self.solve_generics(module, sig, generics, args, expected);
        // Hand the solution to lowering, which needs the *types* the parameters were bound
        // to in order to lay the instance out.
        let solved: Vec<(String, TyId)> = sig
            .generics
            .iter()
            .filter_map(|g| {
                let n = self.env.solver.t.name(g);
                subst.get(&n).map(|&t| (g.clone(), t))
            })
            .collect();
        self.result.set_generics(e.id, solved);
        // Discharge each `where T: P`: the type T was bound to must satisfy P here.
        for (param, proto_path) in &sig.wheres {
            let pn = self.env.solver.t.name(param);
            let Some(&concrete) = subst.get(&pn) else { continue };
            if self.env.is_error(concrete) {
                continue;
            }
            let Some(pid) = self.env.lookup_protocol(module, proto_path) else { continue };
            // A marker is answered from structure, so it is checked *here* rather than
            // through `type_satisfies`: this is the only place that knows the enclosing
            // signature's own bounds, and they are what make a still-generic argument
            // satisfiable. `sort[T](xs: List[T]) where T: Ord` calling `max(xs, xs)` passes
            // `List[T]`, which is ordered exactly because `T` is bound here -- ask without
            // that context and it is not.
            let satisfied = if self.env.protocol(pid).is_marker {
                self.is_ordered(concrete)
            } else if super::generic::is_var(&self.env.solver.t, concrete) {
                // A protocol bound on a still-abstract argument is the caller's own bound
                // to discharge, checked where that caller is called.
                continue;
            } else {
                self.env.type_satisfies(concrete, pid)
            };
            if !satisfied {
                let (ty, name) = (self.show(concrete), proto_path.join("::"));
                let kind = if self.env.protocol(pid).is_marker {
                    TypeErrorKind::UnsatisfiedMarker { ty, marker: name }
                } else {
                    TypeErrorKind::UnsatisfiedBound { ty, protocol: name }
                };
                self.error(e.span.clone(), kind);
            }
        }
        for (a, (_, template)) in args.iter().zip(&sig.params) {
            let want = self.env.solver.t.substitute(*template, &subst);
            self.expr(module, a, Some(want));
        }
        for a in args.iter().skip(sig.params.len()) {
            self.expr(module, a, None);
        }
        let throws = self.env.solver.t.substitute(sig.throws, &subst);
        self.note_throw(e.span.clone(), throws, true);
        self.env.solver.t.substitute(sig.ret, &subst)
    }

    /// The substitution for a generic call's type parameters: a turbofish if
    /// present, else inferred from the argument types and the expected result.
    fn solve_generics(
        &mut self,
        module: &[String],
        sig: &super::env::FnSig,
        generics: &[ast::TypeSpec],
        args: &[Expr],
        expected: Option<TyId>,
    ) -> std::collections::HashMap<super::types::NameId, TyId> {
        use std::collections::{HashMap, HashSet};
        let mut subst: HashMap<_, TyId> = HashMap::new();
        let var_names: HashSet<_> =
            sig.generics.iter().map(|g| self.env.solver.t.name(g)).collect();

        if !generics.is_empty() {
            let scope = self.type_scope(module);
            for (g, spec) in sig.generics.iter().zip(generics) {
                let ty = self.env.resolve(&scope, spec);
                let n = self.env.solver.t.name(g);
                subst.insert(n, ty);
            }
            return subst;
        }

        // Top-down before bottom-up: the expected result sets a variable first, and
        // `infer` is first-wins, so the arguments then conform to it rather than
        // widening it. That is what lets `-> List[i64|str] { push(xs, "s") }` widen
        // on request while a bare `push(xs, "s")` pins `T := i64` and rejects the str.
        if let Some(exp) = expected {
            super::generic::infer(&mut self.env.solver.t, sig.ret, exp, &var_names, &mut subst);
        }
        let arg_tys: Vec<TyId> = args.iter().map(|a| self.expr(module, a, None)).collect();
        for ((_, template), &aty) in sig.params.iter().zip(&arg_tys) {
            super::generic::infer(&mut self.env.solver.t, *template, aty, &var_names, &mut subst);
        }
        subst
    }

    /// Call a value. `callee_ty` must be an arrow; `what` names it for diagnostics.
    fn apply(&mut self, module: &[String], e: &Expr, what: &str, callee_ty: TyId, args: &[Expr]) -> TyId {
        if self.env.is_error(callee_ty) {
            for a in args {
                self.expr(module, a, None);
            }
            return self.poison();
        }
        let Some(arrow) = self.env.solver.t.as_arrow(callee_ty) else {
            for a in args {
                self.expr(module, a, None);
            }
            let ty = self.show(callee_ty);
            self.error(e.span.clone(), TypeErrorKind::NotCallable { what: what.to_string(), ty });
            return self.poison();
        };
        if arrow.params.len() != args.len() {
            self.error(
                e.span.clone(),
                TypeErrorKind::Arity {
                    name: what.to_string(),
                    expected: arrow.params.len(),
                    found: args.len(),
                },
            );
        }
        for (a, want) in args.iter().zip(&arrow.params) {
            self.expr(module, a, Some(*want));
        }
        for a in args.iter().skip(arrow.params.len()) {
            self.expr(module, a, None);
        }
        // A call through a value throws what its arrow says — same rule as a direct
        // call: bare outside a `try`, it is an error; inside one, it lands in the sink.
        self.note_throw(e.span.clone(), arrow.throws, true);
        arrow.ret
    }

    fn dispatch_error(&mut self, span: Span, err: DispatchError) {
        let kind = match err {
            DispatchError::UnknownMethod(n) => TypeErrorKind::UnknownName(n),
            DispatchError::Ambiguous { method, protocols } => {
                TypeErrorKind::AmbiguousCall { method, protocols }
            }
            DispatchError::NoImpl { protocol, method, uncovered } => {
                let uncovered = self.show(uncovered);
                TypeErrorKind::NoImpl { protocol, method, uncovered }
            }
            DispatchError::NoReceiver(n) => TypeErrorKind::NoReceiver(n),
        };
        self.error(span, kind);
    }
}

