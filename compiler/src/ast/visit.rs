//! Walking the AST without rebuilding it.
//!
//! Several consumers need to traverse a module and look at every expression: an editor
//! asking which one sits under the cursor, a symbol outline, a folding-range list. Each
//! of them was otherwise going to write the same forty-arm `match`, and the arms are the
//! part that rots — a new `ExprKind` added to `ast.rs` compiles fine against a walk that
//! ignores it, and the omission shows up as one construct that mysteriously has no
//! hover.
//!
//! **This traversal must agree with `ids::Numberer`.** That pass assigns the `ExprId`s a
//! `TypecheckResult` is keyed by, so an expression it descends into and this one does not
//! is an expression with a type nobody can look up. The two are deliberately written in
//! the same order, arm for arm; when you add a variant to `ExprKind`, both non-exhaustive
//! matches below and in `ids.rs` will fail to compile until you do. Keep it that way — do
//! not add a `_ => {}`.

use super::*;

/// A traversal over shared AST references.
///
/// Every method defaults to "descend and do nothing", so an implementor overrides only
/// the node it cares about. Overriding without calling the matching `walk_*` prunes that
/// subtree, which is how an early-exit search stops paying for the rest of the module.
/// The `'a` is the AST's lifetime, not the visitor's: it is what lets an implementor
/// *keep* a reference it was handed rather than only look at it. `innermost_at` below
/// returns a borrow of the module it searched, which is impossible if each callback gets
/// a fresh anonymous lifetime.
pub trait Visitor<'a>: Sized {
    fn decl(&mut self, d: &'a Decl) {
        walk_decl(self, d);
    }
    fn fn_decl(&mut self, f: &'a FnDecl) {
        walk_fn_decl(self, f);
    }
    fn block(&mut self, b: &'a Block) {
        walk_block(self, b);
    }
    fn stmt(&mut self, s: &'a Stmt) {
        walk_stmt(self, s);
    }
    fn pattern(&mut self, p: &'a Pattern) {
        walk_pattern(self, p);
    }
    fn expr(&mut self, e: &'a Expr) {
        walk_expr(self, e);
    }
}

pub fn walk_module<'a, V: Visitor<'a>>(v: &mut V, m: &'a Module) {
    for d in &m.decls {
        v.decl(d);
    }
}

pub fn walk_decl<'a, V: Visitor<'a>>(v: &mut V, d: &'a Decl) {
    match &d.kind {
        DeclKind::Fn(f) => v.fn_decl(f),
        DeclKind::Protocol(p) => {
            for m in &p.methods {
                v.fn_decl(m);
            }
        }
        DeclKind::Impl(i) => {
            for m in &i.methods {
                v.fn_decl(m);
            }
        }
        DeclKind::Mod(m) => {
            for d in &m.decls {
                v.decl(d);
            }
        }
        DeclKind::Const(c) => v.expr(&c.value),
        DeclKind::TestBlock(t) => v.block(&t.body),
        DeclKind::Record(_)
        | DeclKind::TypeAlias(_)
        | DeclKind::MuType(_)
        | DeclKind::Newtype(_)
        | DeclKind::Use(_)
        | DeclKind::Error => {}
    }
}

pub fn walk_fn_decl<'a, V: Visitor<'a>>(v: &mut V, f: &'a FnDecl) {
    if let Some(b) = &f.body {
        v.block(b);
    }
}

pub fn walk_block<'a, V: Visitor<'a>>(v: &mut V, b: &'a Block) {
    for s in &b.stmts {
        v.stmt(s);
    }
    if let Some(t) = &b.tail {
        v.expr(t);
    }
}

pub fn walk_stmt<'a, V: Visitor<'a>>(v: &mut V, s: &'a Stmt) {
    match &s.kind {
        StmtKind::Let { pat, value, .. } => {
            v.pattern(pat);
            v.expr(value);
        }
        StmtKind::Assign { value, .. } => v.expr(value),
        StmtKind::Expr(e) => v.expr(e),
        StmtKind::Error => {}
    }
}

pub fn walk_pattern<'a, V: Visitor<'a>>(v: &mut V, p: &'a Pattern) {
    match &p.kind {
        PatternKind::Literal(e) => v.expr(e),
        PatternKind::Record { fields, .. } => {
            for f in fields {
                if let Some(p) = &f.pat {
                    v.pattern(p);
                }
            }
        }
        PatternKind::Tuple(xs) => {
            for p in xs {
                v.pattern(p);
            }
        }
        PatternKind::Is(_)
        | PatternKind::Wildcard
        | PatternKind::Bind(_)
        | PatternKind::Error => {}
    }
}

pub fn walk_expr<'a, V: Visitor<'a>>(v: &mut V, e: &'a Expr) {
    match &e.kind {
        ExprKind::Todo(_) => {}

        ExprKind::Str(parts) => {
            for p in parts {
                if let StrPart::Interp(e) = p {
                    v.expr(e);
                }
            }
        }
        ExprKind::Unary { rhs, .. } => v.expr(rhs),
        ExprKind::Binary { lhs, rhs, .. } => {
            v.expr(lhs);
            v.expr(rhs);
        }
        ExprKind::Call { callee, args, .. } => {
            v.expr(callee);
            for a in args {
                v.expr(a);
            }
        }
        ExprKind::Index { base, index } => {
            v.expr(base);
            v.expr(index);
        }
        ExprKind::Field { base, .. } => v.expr(base),
        ExprKind::List(elems) => {
            for el in elems {
                match el {
                    Elem::Value(e) | Elem::Spread(e) => v.expr(e),
                }
            }
        }
        ExprKind::RecordLit { fields, spread, .. } => {
            for f in fields {
                v.expr(&f.value);
            }
            if let Some(s) = spread {
                v.expr(s);
            }
        }
        ExprKind::Tuple(xs) => {
            for e in xs {
                v.expr(e);
            }
        }
        ExprKind::Lambda { body, .. } => v.expr(body),
        ExprKind::If { cond, then, else_ } => {
            v.expr(cond);
            v.block(then);
            if let Some(e) = else_ {
                v.expr(e);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            v.expr(scrutinee);
            for a in arms {
                v.pattern(&a.pat);
                if let Some(g) = &a.guard {
                    v.expr(g);
                }
                v.expr(&a.body);
            }
        }
        ExprKind::Block(b) => v.block(b),
        ExprKind::Loop { body } => v.block(body),
        ExprKind::While { cond, body } => {
            v.expr(cond);
            v.block(body);
        }
        ExprKind::For { pat, iter, body } => {
            v.pattern(pat);
            v.expr(iter);
            v.block(body);
        }
        ExprKind::Break(x) | ExprKind::Return(x) => {
            if let Some(e) = x {
                v.expr(e);
            }
        }
        ExprKind::Throw(e) => v.expr(e),
        ExprKind::Try { body, catch, .. } => {
            v.expr(body);
            if let Some(c) = catch {
                v.block(&c.body);
            }
        }
        ExprKind::Is { lhs, .. } | ExprKind::As { lhs, .. } => v.expr(lhs),
        ExprKind::Assert { args, .. } => {
            for a in args {
                v.expr(a);
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

/// Call `f` on every expression in the module, outermost first.
///
/// The closure form, for the majority of callers that want one arm rather than a trait
/// implementation.
pub fn each_expr<'a>(m: &'a Module, f: impl FnMut(&'a Expr)) {
    struct W<F>(F);
    impl<'a, F: FnMut(&'a Expr)> Visitor<'a> for W<F> {
        fn expr(&mut self, e: &'a Expr) {
            (self.0)(e);
            walk_expr(self, e);
        }
    }
    walk_module(&mut W(f), m);
}

/// Call `f` on every pattern in the module.
///
/// Separate from `each_expr` because patterns are a different node type that happens to
/// share the id space; a caller looking for bindings wants these and not expressions.
pub fn each_pattern<'a>(m: &'a Module, f: impl FnMut(&'a Pattern)) {
    struct W<F>(F);
    impl<'a, F: FnMut(&'a Pattern)> Visitor<'a> for W<F> {
        fn pattern(&mut self, p: &'a Pattern) {
            (self.0)(p);
            walk_pattern(self, p);
        }
    }
    walk_module(&mut W(f), m);
}

/// The innermost expression whose span contains `offset`, if any.
///
/// "Innermost" is by span width rather than by tree depth, and the difference is not
/// academic: a `Call`'s span covers its callee and arguments, so a cursor on an argument
/// is inside both, and depth alone does not say which the user pointed at. The narrowest
/// containing span is the one they clicked.
///
/// Ties go to the later candidate, which is the deeper one — a node sharing its parent's
/// exact span (a block's tail expression, say) is the more specific answer.
pub fn innermost_at(m: &Module, offset: usize) -> Option<&Expr> {
    let mut best: Option<&Expr> = None;
    each_expr(m, |e| {
        if e.span.contains(&offset) {
            let width = e.span.end - e.span.start;
            if best.is_none_or(|b| width <= b.span.end - b.span.start) {
                best = Some(e);
            }
        }
    });
    best
}

/// The innermost pattern whose span contains `offset`.
///
/// A cursor on a binding site is on a `Pattern`, not an `Expr` — the two spaces do not
/// overlap, so a caller wanting "whatever is under the cursor" asks both.
pub fn innermost_pattern_at(m: &Module, offset: usize) -> Option<&Pattern> {
    let mut best: Option<&Pattern> = None;
    each_pattern(m, |p| {
        if p.span.contains(&offset) {
            let width = p.span.end - p.span.start;
            if best.is_none_or(|b| width <= b.span.end - b.span.start) {
                best = Some(p);
            }
        }
    });
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lexer, parser};

    fn parse(src: &str) -> Module {
        let tokens = lexer::lex(src).expect("the fixture lexes");
        let (m, errs) = parser::parse(&tokens, src.len());
        assert!(errs.is_empty(), "parse errors: {errs:?}");
        let mut m = m.expect("the fixture parses");
        super::super::number_exprs(&mut m);
        m
    }

    /// The property that matters: this walk reaches exactly what `ids.rs` numbered.
    /// If it reached fewer, those expressions would have types no consumer could find;
    /// if more, it would be visiting nodes the checker never saw.
    #[test]
    fn the_walk_reaches_every_numbered_node() {
        let src = "fn f(a: i64) -> i64 {
             let xs = [1, 2, 3];
             let m = match a { 1 => \"one\", _ => \"many\" };
             for x in xs { println(\"#{x} #{m}\") }
             if a > 0 { a } else { -a }
         }";
        let m = parse(src);

        let mut seen = std::collections::HashSet::new();
        each_expr(&m, |e| {
            assert!(seen.insert(e.id), "expression id {:?} visited twice", e.id);
        });
        each_pattern(&m, |p| {
            assert!(seen.insert(p.id), "pattern id {:?} visited twice", p.id);
        });

        // `number_exprs` hands out 0..n contiguously, so a complete walk sees each once.
        let n = seen.len() as u32;
        for i in 0..n {
            assert!(seen.contains(&ExprId(i)), "id {i} was numbered but never visited");
        }
    }

    #[test]
    fn the_innermost_expression_wins() {
        let src = "fn f() -> i64 { foo(bar) }";
        let m = parse(src);
        let at = src.find("bar").expect("the fixture contains `bar`");
        let e = innermost_at(&m, at).expect("something is under the cursor");
        assert_eq!(&src[e.span.clone()], "bar");
    }

    #[test]
    fn a_cursor_on_the_callee_finds_the_callee_not_the_call() {
        let src = "fn f() -> i64 { foo(bar) }";
        let m = parse(src);
        let at = src.find("foo").expect("the fixture contains `foo`");
        let e = innermost_at(&m, at).expect("something is under the cursor");
        assert_eq!(&src[e.span.clone()], "foo");
    }

    #[test]
    fn an_offset_outside_every_span_finds_nothing() {
        let m = parse("fn f() -> i64 { 1 }");
        assert!(innermost_at(&m, 0).is_none(), "`fn` is not inside any expression");
    }

    #[test]
    fn a_binding_site_is_found_as_a_pattern() {
        let src = "fn f() { let count = 1; }";
        let m = parse(src);
        let at = src.find("count").expect("the fixture contains `count`");
        let p = innermost_pattern_at(&m, at).expect("the binding is a pattern");
        assert_eq!(&src[p.span.clone()], "count");
    }
}
