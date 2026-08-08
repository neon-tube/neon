//! Assigning stable identity to every expression.
//!
//! The checker records a type per expression keyed on `ExprId`. Throwing those
//! types away is what forced the previous implementation to re-derive them during
//! lowering, fail, and fall back to erasure — so the key has to be sound. Spans
//! cannot do it: they are what formatting is allowed to change, and nothing stops
//! two nodes sharing one.

use super::*;

/// Number every expression in pre-order. Returns how many there were.
/// Number every expression in a module, starting at `base`, and return the next free id.
///
/// `base` exists because ids must be unique across *all* modules in a compilation, not just
/// within one: the stdlib is real Neon code whose bodies are checked and lowered alongside
/// the program, and a `TypecheckResult` is keyed by `ExprId`. Numbering each module from
/// zero would make those keys collide.
pub fn number_exprs_from(module: &mut Module, base: u32) -> u32 {
    let mut n = Numberer { next: base };
    for d in &mut module.decls {
        n.decl(d);
    }
    n.next
}

/// Number a standalone module from zero.
pub fn number_exprs(module: &mut Module) -> u32 {
    number_exprs_from(module, 0)
}

struct Numberer {
    next: u32,
}

impl Numberer {
    fn decl(&mut self, d: &mut Decl) {
        match &mut d.kind {
            DeclKind::Fn(f) => self.fn_decl(f),
            DeclKind::Protocol(p) => {
                for m in &mut p.methods {
                    self.fn_decl(m);
                }
            }
            DeclKind::Impl(i) => {
                for m in &mut i.methods {
                    self.fn_decl(m);
                }
            }
            DeclKind::Mod(m) => {
                for d in &mut m.decls {
                    self.decl(d);
                }
            }
            DeclKind::Const(c) => self.expr(&mut c.value),
            DeclKind::TestBlock(t) => self.block(&mut t.body),
            DeclKind::Record(_)
            | DeclKind::TypeAlias(_)
            | DeclKind::MuType(_)
            | DeclKind::Newtype(_)
            | DeclKind::Use(_)
            | DeclKind::Error => {}
        }
    }

    fn fn_decl(&mut self, f: &mut FnDecl) {
        if let Some(b) = &mut f.body {
            self.block(b);
        }
    }

    fn block(&mut self, b: &mut Block) {
        for s in &mut b.stmts {
            self.stmt(s);
        }
        if let Some(t) = &mut b.tail {
            self.expr(t);
        }
    }

    fn stmt(&mut self, s: &mut Stmt) {
        match &mut s.kind {
            StmtKind::Let { pat, value, .. } => {
                self.pattern(pat);
                self.expr(value);
            }
            StmtKind::Assign { value, .. } => self.expr(value),
            StmtKind::Expr(e) => self.expr(e),
            StmtKind::Error => {}
        }
    }

    fn pattern(&mut self, p: &mut Pattern) {
        // Patterns share the expression counter. They are keys into the same
        // `TypecheckResult`, so the two spaces must not collide.
        p.id = ExprId(self.next);
        self.next += 1;
        match &mut p.kind {
            PatternKind::Literal(e) => self.expr(e),
            PatternKind::Record { fields, .. } => {
                for f in fields {
                    if let Some(p) = &mut f.pat {
                        self.pattern(p);
                    }
                }
            }
            PatternKind::Tuple(v) => {
                for p in v {
                    self.pattern(p);
                }
            }
            PatternKind::Is(_)
            | PatternKind::Wildcard
            | PatternKind::Bind(_)
            | PatternKind::Error => {}
        }
    }

    fn expr(&mut self, e: &mut Expr) {
        e.id = ExprId(self.next);
        self.next += 1;
        match &mut e.kind {
            ExprKind::Todo(_) => {}

            ExprKind::Str(parts) => {
                for p in parts {
                    if let StrPart::Interp(e) = p {
                        self.expr(e);
                    }
                }
            }
            ExprKind::Unary { rhs, .. } => self.expr(rhs),
            ExprKind::Binary { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            ExprKind::Call { callee, args, .. } => {
                self.expr(callee);
                for a in args {
                    self.expr(a);
                }
            }
            ExprKind::Index { base, index } => {
                self.expr(base);
                self.expr(index);
            }
            ExprKind::Field { base, .. } => self.expr(base),
            ExprKind::List(elems) => {
                for el in elems {
                    match el {
                        Elem::Value(e) | Elem::Spread(e) => self.expr(e),
                    }
                }
            }
            ExprKind::RecordLit { fields, spread, .. } => {
                for f in fields {
                    self.expr(&mut f.value);
                }
                if let Some(s) = spread {
                    self.expr(s);
                }
            }
            ExprKind::Tuple(v) => {
                for e in v {
                    self.expr(e);
                }
            }
            ExprKind::Lambda { body, .. } => self.expr(body),
            ExprKind::If { cond, then, else_ } => {
                self.expr(cond);
                self.block(then);
                if let Some(e) = else_ {
                    self.expr(e);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for a in arms {
                    self.pattern(&mut a.pat);
                    if let Some(g) = &mut a.guard {
                        self.expr(g);
                    }
                    self.expr(&mut a.body);
                }
            }
            ExprKind::Block(b) => self.block(b),
            ExprKind::Loop { body } => self.block(body),
            ExprKind::While { cond, body } => {
                self.expr(cond);
                self.block(body);
            }
            ExprKind::For { pat, iter, body } => {
                self.pattern(pat);
                self.expr(iter);
                self.block(body);
            }
            ExprKind::Break(v) | ExprKind::Return(v) => {
                if let Some(e) = v {
                    self.expr(e);
                }
            }
            ExprKind::Throw(e) => self.expr(e),
            ExprKind::Try { body, catch, .. } => {
                self.expr(body);
                if let Some(c) = catch {
                    self.block(&mut c.body);
                }
            }
            ExprKind::Is { lhs, .. } | ExprKind::As { lhs, .. } => self.expr(lhs),
            ExprKind::Assert { args, .. } => {
                for a in args {
                    self.expr(a);
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
}
