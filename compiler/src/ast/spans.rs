//! Zeroing spans, so two trees can be compared for meaning rather than position.
//!
//! `parse(format(src)) == parse(src)` is the property that says formatting never
//! changes what a program means, and it can only be checked once the one thing
//! formatting is *supposed* to change — where each node sits — is taken out of
//! the comparison.
//!
//! The walk below must stay **total**: every field that carries a `Span`, at every
//! depth, has to be visited. A missed one does not fail loudly — it leaves a real
//! position in one tree and a real position in the other, and the equality test
//! then reports a difference that formatting was entitled to make. The result is a
//! round-trip test that fails on correct output, so the pressure is to weaken the
//! test rather than fix the walk. When a node gains a span-bearing field, it gains
//! a line here.
//!
//! `ExprId` is deliberately *not* zeroed. Ids come from `number_exprs`, which the
//! round-trip callers do not run, so both sides hold `ExprId::UNSET` and compare
//! equal. Comparing two trees that have been numbered would need ids stripped too:
//! they are assigned in pre-order, so any node added or removed renumbers
//! everything after it.

use super::*;

const ZERO: Span = 0..0;

/// Set every span in the tree to `0..0`, in place.
pub fn strip_spans(module: &mut Module) {
    for d in &mut module.decls {
        decl(d);
    }
}

/// The shapes that look like omissions but are not: an `AliasDecl` and a `ConstDecl` have
/// no span field, and a `UseDecl`'s `UseTree` carries its positions on the `UseDecl`
/// itself rather than per node, so one assignment covers the whole tree.
fn decl(d: &mut Decl) {
    d.span = ZERO;
    match &mut d.kind {
        DeclKind::Fn(f) => fn_decl(f),
        DeclKind::Record(r) => {
            for a in &mut r.annotations {
                a.span = ZERO;
            }
            for f in &mut r.fields {
                field(f);
            }
        }
        DeclKind::Protocol(p) => {
            for m in &mut p.methods {
                fn_decl(m);
            }
        }
        DeclKind::Impl(i) => {
            ty(&mut i.target);
            for m in &mut i.methods {
                fn_decl(m);
            }
        }
        DeclKind::TypeAlias(a) | DeclKind::MuType(a) | DeclKind::Newtype(a) => ty(&mut a.value),
        DeclKind::Use(u) => u.span = ZERO,
        DeclKind::Mod(m) => {
            for d in &mut m.decls {
                decl(d);
            }
        }
        DeclKind::Const(c) => {
            if let Some(t) = &mut c.ty {
                ty(t);
            }
            expr(&mut c.value);
        }
        DeclKind::TestBlock(t) => block(&mut t.body),
        DeclKind::Error => {}
    }
}

/// A `FnDecl` carries no span of its own — the enclosing `Decl` holds it, and a protocol
/// or impl method has none at all — so this only descends. Every position a diagnostic
/// about a signature can point at lives on a `Param`, an `Annotation` or a `TypeSpec`.
fn fn_decl(f: &mut FnDecl) {
    for a in &mut f.annotations {
        a.span = ZERO;
    }
    for p in &mut f.params {
        p.span = ZERO;
        ty(&mut p.ty);
    }
    if let Some(t) = &mut f.throws {
        ty(t);
    }
    if let Some(t) = &mut f.ret {
        ty(t);
    }
    for w in &mut f.wheres {
        ty(&mut w.bound);
    }
    if let Some(b) = &mut f.body {
        block(b);
    }
}

fn field(f: &mut Field) {
    f.span = ZERO;
    ty(&mut f.ty);
}

fn ty(t: &mut TypeSpec) {
    t.span = ZERO;
    match &mut t.kind {
        TypeSpecKind::Named { args, .. } => {
            for a in args {
                ty(a);
            }
        }
        TypeSpecKind::Struct(fields) => {
            for f in fields {
                field(f);
            }
        }
        TypeSpecKind::Union(v) | TypeSpecKind::Intersect(v) | TypeSpecKind::Tuple(v) => {
            for t in v {
                ty(t);
            }
        }
        TypeSpecKind::Negate(t) => ty(t),
        TypeSpecKind::Fn { params, throws, ret } => {
            for p in params {
                ty(p);
            }
            if let Some(t) = throws {
                ty(t);
            }
            ty(ret);
        }
        TypeSpecKind::Atom(_) | TypeSpecKind::Null | TypeSpecKind::Any | TypeSpecKind::Error => {}
    }
}

fn block(b: &mut Block) {
    b.span = ZERO;
    for s in &mut b.stmts {
        stmt(s);
    }
    if let Some(t) = &mut b.tail {
        expr(t);
    }
}

fn stmt(s: &mut Stmt) {
    s.span = ZERO;
    match &mut s.kind {
        StmtKind::Let { pat, ty: t, value } => {
            pattern(pat);
            if let Some(t) = t {
                ty(t);
            }
            expr(value);
        }
        StmtKind::Assign { value, .. } => expr(value),
        StmtKind::Expr(e) => expr(e),
        StmtKind::Error => {}
    }
}

fn pattern(p: &mut Pattern) {
    p.span = ZERO;
    match &mut p.kind {
        PatternKind::Is(t) => ty(t),
        PatternKind::Literal(e) => expr(e),
        PatternKind::Record { fields, .. } => {
            for f in fields {
                f.span = ZERO;
                if let Some(p) = &mut f.pat {
                    pattern(p);
                }
            }
        }
        PatternKind::Tuple(v) => {
            for p in v {
                pattern(p);
            }
        }
        PatternKind::Wildcard | PatternKind::Bind(_) | PatternKind::Error => {}
    }
}

fn expr(e: &mut Expr) {
    e.span = ZERO;
    match &mut e.kind {
        ExprKind::Str(parts) => {
            for p in parts {
                if let StrPart::Interp(e) = p {
                    expr(e);
                }
            }
        }
        ExprKind::Unary { rhs, .. } => expr(rhs),
        ExprKind::Binary { lhs, rhs, .. } => {
            expr(lhs);
            expr(rhs);
        }
        ExprKind::Call { callee, generics, args } => {
            expr(callee);
            for g in generics {
                ty(g);
            }
            for a in args {
                expr(a);
            }
        }
        ExprKind::Index { base, index } => {
            expr(base);
            expr(index);
        }
        ExprKind::Field { base, .. } => expr(base),
        ExprKind::List(elems) => {
            for el in elems {
                match el {
                    Elem::Value(e) | Elem::Spread(e) => expr(e),
                }
            }
        }
        ExprKind::RecordLit { fields, spread, .. } => {
            for f in fields {
                f.span = ZERO;
                expr(&mut f.value);
            }
            if let Some(s) = spread {
                expr(s);
            }
        }
        ExprKind::Tuple(v) => {
            for e in v {
                expr(e);
            }
        }
        ExprKind::Lambda { params, body } => {
            for p in params {
                if let Some(t) = &mut p.ty {
                    ty(t);
                }
            }
            expr(body);
        }
        ExprKind::If { cond, then, else_ } => {
            expr(cond);
            block(then);
            if let Some(e) = else_ {
                expr(e);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            expr(scrutinee);
            for a in arms {
                a.span = ZERO;
                pattern(&mut a.pat);
                if let Some(g) = &mut a.guard {
                    expr(g);
                }
                expr(&mut a.body);
            }
        }
        ExprKind::Block(b) => block(b),
        ExprKind::Loop { body } => block(body),
        ExprKind::While { cond, body } => {
            expr(cond);
            block(body);
        }
        ExprKind::For { pat, iter, body } => {
            pattern(pat);
            expr(iter);
            block(body);
        }
        ExprKind::Break(v) | ExprKind::Return(v) => {
            if let Some(e) = v {
                expr(e);
            }
        }
        ExprKind::Throw(e) => expr(e),
        ExprKind::Try { body, catch, .. } => {
            expr(body);
            if let Some(c) = catch {
                c.span = ZERO;
                block(&mut c.body);
            }
        }
        ExprKind::Is { lhs, ty: t } | ExprKind::As { lhs, ty: t, .. } => {
            expr(lhs);
            ty(t);
        }
        ExprKind::Assert { args, .. } => {
            for a in args {
                expr(a);
            }
        }
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Rune(_)
        | ExprKind::Atom(_)
        | ExprKind::Bool(_)
        | ExprKind::Null
        | ExprKind::Path(_)
        | ExprKind::Continue
        | ExprKind::Error => {}
    }
}
