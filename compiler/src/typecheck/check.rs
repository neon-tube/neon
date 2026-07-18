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
    let mut c = Checker {
        env,
        result: TypecheckResult::default(),
        errors: vec![],
        locals: vec![],
        ret: None,
        throws: None,
        loop_breaks: vec![],
    };
    c.decls(&[], &m.decls);
    (c.result, c.errors)
}

struct Checker<'a> {
    env: &'a mut Env,
    result: TypecheckResult,
    errors: Vec<TypeError>,
    /// Innermost last. A name resolves to the nearest binding.
    locals: Vec<Vec<(String, TyId)>>,
    ret: Option<TyId>,
    throws: Option<TyId>,
    /// Break values of the enclosing loops, innermost last. A `loop` is the union
    /// of the values it breaks with.
    loop_breaks: Vec<Vec<TyId>>,
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
                ast::DeclKind::Fn(f) => self.fn_body(module, f, &[]),
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
                    self.throws = Some(never);
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

        self.locals.push(vec![]);
        for p in &f.params {
            let t = self.env.resolve(&scope, &p.ty);
            self.bind(&p.name, t);
        }

        let ret = match &f.ret {
            Some(t) => self.env.resolve(&scope, t),
            None => self.env.solver.t.tuple(vec![]),
        };
        let throws = match &f.throws {
            Some(t) => self.env.resolve(&scope, t),
            None => self.env.solver.t.never(),
        };
        self.ret = Some(ret);
        self.throws = Some(throws);

        // A body-less `-> ()` fn is a statement sequence; anything else must
        // produce its return type as the tail.
        let unit = self.env.solver.t.tuple(vec![]);
        let want = if ret == unit { None } else { Some(ret) };
        self.block(module, body, want);
        self.locals.pop();
    }

    // ---- scopes ----

    fn bind(&mut self, name: &str, t: TyId) {
        if let Some(scope) = self.locals.last_mut() {
            scope.push((name.to_string(), t));
        }
    }

    fn lookup(&self, name: &str) -> Option<TyId> {
        self.locals.iter().rev().flat_map(|s| s.iter().rev()).find(|(n, _)| n == name).map(|(_, t)| *t)
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

    fn stmt(&mut self, module: &[String], s: &ast::Stmt) {
        match &s.kind {
            ast::StmtKind::Let { pat, ty, value } => {
                let scope = Scope::new(module);
                let want = ty.as_ref().map(|t| self.env.resolve(&scope, t));
                let t = self.expr(module, value, want);
                // The annotation is the binding's type when there is one: `let x:
                // i64|str = 1` binds the wider type, not `i64`.
                self.bind_pattern(pat, want.unwrap_or(t));
            }
            ast::StmtKind::Assign { name, value } => {
                let Some(want) = self.lookup(name) else {
                    self.error(s.span.clone(), TypeErrorKind::UnknownName(name.clone()));
                    self.expr(module, value, None);
                    return;
                };
                self.expr(module, value, Some(want));
            }
            ast::StmtKind::Expr(e) => {
                self.expr(module, e, None);
            }
            ast::StmtKind::Error => {}
        }
    }

    fn bind_pattern(&mut self, p: &ast::Pattern, t: TyId) {
        match &p.kind {
            ast::PatternKind::Bind(n) => self.bind(n, t),
            ast::PatternKind::Wildcard => {}
            ast::PatternKind::Tuple(ps) => {
                for (i, sub) in ps.iter().enumerate() {
                    let e = narrow::project_elem(&mut self.env.solver, t, i);
                    let et = self.projected(sub.span.clone(), e, &i.to_string(), t);
                    self.bind_pattern(sub, et);
                }
            }
            ast::PatternKind::Record { fields, .. } => {
                for f in fields {
                    let label = self.env.solver.t.name(&f.name);
                    let pj = narrow::project_field(&mut self.env.solver, t, label);
                    let ft = self.projected(p.span.clone(), pj, &f.name, t);
                    match &f.pat {
                        Some(sub) => self.bind_pattern(sub, ft),
                        None => self.bind(&f.name, ft),
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
                for el in elems {
                    match el {
                        ast::Elem::Value(x) | ast::Elem::Spread(x) => {
                            self.expr(module, x, None);
                        }
                    }
                }
                // A list's type needs `List[T]`, which needs the stdlib. Until `use`
                // loads it, saying anything here would be a guess.
                expected.unwrap_or_else(|| self.poison())
            }

            ExprKind::If { cond, then, else_ } => self.if_expr(module, e, cond, then, else_, expected),

            ExprKind::Match { scrutinee, arms } => self.match_expr(module, e, scrutinee, arms, expected),

            ExprKind::Block(b) => self.block(module, b, expected),

            ExprKind::Is { lhs, ty } => {
                self.expr(module, lhs, None);
                let scope = Scope::new(module);
                self.env.resolve(&scope, ty);
                self.env.solver.t.bool()
            }

            ExprKind::As { lhs, ty } => {
                let from = self.expr(module, lhs, None);
                let scope = Scope::new(module);
                let to = self.env.resolve(&scope, ty);
                // A cast narrows; it cannot reach a type the value could never be.
                let meet = self.env.solver.t.intersect(from, to);
                if !self.env.is_error(from) && self.env.solver.is_empty(meet) {
                    let (f, t) = (self.show(from), self.show(to));
                    self.error(e.span.clone(), TypeErrorKind::ImpossibleCast { from: f, to: t });
                    return self.poison();
                }
                to
            }

            ExprKind::Return(v) => {
                let want = self.ret;
                if let Some(x) = v {
                    self.expr(module, x, want);
                }
                self.env.solver.t.never()
            }

            ExprKind::Throw(x) => {
                let want = self.throws;
                self.expr(module, x, want);
                self.env.solver.t.never()
            }

            ExprKind::Break(v) => {
                let t = match v {
                    Some(x) => self.expr(module, x, None),
                    None => self.env.solver.t.tuple(vec![]),
                };
                if let Some(breaks) = self.loop_breaks.last_mut() {
                    breaks.push(t);
                }
                self.env.solver.t.never()
            }
            ExprKind::Continue => self.env.solver.t.never(),

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
                self.locals.push(vec![]);
                self.bind_pattern(pat, t);
                self.block(module, body, None);
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
                let label = self.env.solver.t.name(name);
                let p = narrow::project_field(&mut self.env.solver, t, label);
                self.projected(e.span.clone(), p, name, t)
            }

            ExprKind::Error => self.poison(),

            ExprKind::Lambda { params, body } => self.lambda(module, e, params, body, expected),

            // Not yet: each needs something that does not exist. A guess here is
            // exactly the fallback this design has no room for.
            ExprKind::Index { .. } | ExprKind::RecordLit { .. } | ExprKind::Try { .. } => {
                expected.unwrap_or_else(|| self.poison())
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
        let scope = Scope::new(module);
        let want = expected.and_then(|t| self.env.solver.t.as_arrow(t));

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
            self.bind(&p.name, t);
            param_tys.push(t);
        }

        // A lambda has no `throws` clause, so it cannot throw. Check its body in a
        // context that says so, and restore the enclosing one after.
        let want_ret = want.as_ref().map(|a| a.ret);
        let never = self.env.solver.t.never();
        let saved = self.throws.replace(never);
        let ret = self.expr(module, body, want_ret);
        self.throws = saved;
        self.locals.pop();

        let arrow = self.env.solver.t.arrow(param_tys, never, ret);
        self.result.set_lambda(e.id, arrow);
        arrow
    }

    fn path(&mut self, module: &[String], e: &Expr, p: &[String]) -> TyId {
        if let [one] = p {
            if let Some(t) = self.lookup(one) {
                return t;
            }
        }
        let joined = p.join("::");
        if let Some(sig) = self.env.fn_named(module, p) {
            return sig.ty;
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
            BinOp::Eq | BinOp::Ne => {
                self.expr(module, lhs, None);
                self.expr(module, rhs, None);
                self.env.solver.t.bool()
            }
            // Ordering needs an order. `1 < 2` and `"a" < "b"` are fine; `1 < "s"`
            // has no common ordered type, and atoms have no order at all.
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let l = self.expr(module, lhs, None);
                let r = self.expr(module, rhs, None);
                let meet = self.env.solver.t.intersect(l, r);
                if !self.env.is_error(l) && !self.env.is_error(r) && self.env.solver.is_empty(meet) {
                    let (a, b) = (self.show(l), self.show(r));
                    self.error(e.span.clone(), TypeErrorKind::Incomparable { left: a, right: b });
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
            let t = self.block(module, then, None);
            // Consumed as a value, an `if` with no `else` has nothing to be when the
            // condition is false. The parser records the absence rather than
            // substituting null, so the checker is the one that must say so.
            if expected.is_some() {
                self.error(e.span.clone(), TypeErrorKind::IfWithoutElse);
                return self.poison();
            }
            let _ = t;
            return self.env.solver.t.tuple(vec![]);
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
        let mut covered: Vec<narrow::Test> = vec![];
        let mut result = self.env.solver.t.never();

        for arm in arms {
            let test = self.arm_test(module, arm, subject);
            self.locals.push(vec![]);
            let bound = match test {
                Some(t) => self.env.solver.t.intersect(subject, t.ty),
                None => subject,
            };
            self.bind_pattern(&arm.pat, bound);
            if let Some(g) = &arm.guard {
                let b = self.env.solver.t.bool();
                self.expr(module, g, Some(b));
            }
            let t = self.expr(module, &arm.body, expected);
            self.locals.pop();
            result = self.union_branches(result, t);

            if let Some(mut test) = test {
                if arm.guard.is_some() {
                    test = test.guarded();
                }
                covered.push(test);
            }
        }

        // Exhaustiveness falls out: the residual is a type naming exactly what was
        // missed. Only EXACT arms count — `1` is an i64 but matches one i64, so
        // counting it as covering i64 would report this exhaustive.
        let covered: Vec<TyId> =
            covered.into_iter().map(|t| t.covered(&mut self.env.solver)).collect();
        let rest = narrow::residual(&mut self.env.solver, subject, &covered);
        if !self.env.solver.is_empty(rest) && !self.env.is_error(subject) {
            let missing = self.show(rest);
            self.error(e.span.clone(), TypeErrorKind::NotExhaustive { missing });
        }
        result
    }

    /// What an arm tests for, or `None` when it is a plain binding (which admits
    /// everything, and so is not a test at all).
    fn arm_test(&mut self, module: &[String], arm: &ast::MatchArm, subject: TyId) -> Option<narrow::Test> {
        let scope = Scope::new(module);
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
            _ => None,
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

        let arg_tys: Vec<TyId> = args.iter().map(|a| self.expr(module, a, None)).collect();
        let (name, qualified) = match p.split_last() {
            // A bare name may have been imported as a specific protocol's method.
            Some((last, [])) => (last.clone(), self.env.imported_method(module, last)),
            Some((last, rest)) => (last.clone(), self.env.lookup_protocol(module, rest)),
            None => return self.poison(),
        };

        match dispatch::resolve(self.env, &name, qualified, &arg_tys, expected) {
            Ok(s) => {
                self.result.set_call(e.id, s.resolution);
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
            return sig.ret;
        }

        // A generic fn: solve its type parameters, then check under the solution.
        let subst = self.solve_generics(module, sig, generics, args, expected);
        for (a, (_, template)) in args.iter().zip(&sig.params) {
            let want = self.env.solver.t.substitute(*template, &subst);
            self.expr(module, a, Some(want));
        }
        for a in args.iter().skip(sig.params.len()) {
            self.expr(module, a, None);
        }
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
            let scope = Scope::new(module);
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
        arrow.ret
    }

    fn dispatch_error(&mut self, span: Span, err: DispatchError) {
        let kind = match err {
            DispatchError::UnknownMethod(n) => TypeErrorKind::UnknownName(n),
            DispatchError::Ambiguous { method, protocols } => {
                TypeErrorKind::AmbiguousCall { method, protocols }
            }
            DispatchError::NoImpl { protocol, uncovered } => {
                let uncovered = self.show(uncovered);
                TypeErrorKind::NoImpl { protocol, uncovered }
            }
            DispatchError::NoReceiver(n) => TypeErrorKind::NoReceiver(n),
        };
        self.error(span, kind);
    }
}

