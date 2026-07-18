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

/// Lower a whole module to a program of SSA functions.
pub fn lower_module(env: &Env, result: &TypecheckResult, module: &ast::Module) -> Program {
    let mut funcs = Vec::new();
    lower_decls(env, result, &[], &module.decls, &mut funcs);
    Program { funcs }
}

fn lower_decls(
    env: &Env,
    result: &TypecheckResult,
    module: &[String],
    decls: &[Decl],
    out: &mut Vec<Func>,
) {
    for d in decls {
        match &d.kind {
            DeclKind::Fn(f) if f.body.is_some() => {
                out.push(lower_fn(env, result, module, f));
            }
            DeclKind::Mod(m) => {
                let mut inner = module.to_vec();
                inner.push(m.name.clone());
                lower_decls(env, result, &inner, &m.decls, out);
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

fn lower_fn(env: &Env, result: &TypecheckResult, module: &[String], f: &ast::FnDecl) -> Func {
    let name = f.name.clone();
    let sig = env.fn_named(module, std::slice::from_ref(&name));
    let ret_ty = sig.map(|s| s.ret).unwrap_or(TyId(0));
    let ret_repr = repr_of(&env.solver.t, ret_ty);

    let mut lo = Lower {
        env,
        result,
        module: module.to_vec(),
        b: Builder::new(mangle(module, &f.name), ret_repr.clone()),
        scope: vec![vec![]],
        terminated: false,
    };

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
    lo.b.finish(params)
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
            Some(ty) => repr_of(&self.env.solver.t, ty),
            None => Repr::Unit,
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
            ExprKind::Block(b) => self.lower_block(b).unwrap_or_else(|| self.unit(ty)),
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

    fn lower_str(&mut self, parts: &[ast::StrPart], repr: Repr, ty: TyId) -> Value {
        // A single literal chunk is a constant; interpolation is not lowered yet.
        match parts {
            [] => self.b.emit(Op::ConstStr(String::new()), repr, ty),
            [ast::StrPart::Text(s)] => self.b.emit(Op::ConstStr(s.clone()), repr, ty),
            _ => self.unhandled_note("string interpolation", repr, ty),
        }
    }

    fn lower_path(&mut self, p: &[String], repr: Repr, ty: TyId) -> Value {
        if let [name] = p {
            if let Some(v) = self.lookup(name) {
                return v;
            }
        }
        // A bare reference to a function as a value is not lowered yet.
        self.unhandled_note("path-as-value", repr, ty)
    }

    fn lower_binary(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr, repr: Repr, ty: TyId) -> Value {
        match bin_prim(op) {
            Some(p) => {
                let l = self.lower_expr(lhs);
                let r = self.lower_expr(rhs);
                self.b.emit(Op::Prim(p, vec![l, r]), repr, ty)
            }
            None => self.unhandled_note("binary op (orelse/pipe/short-circuit)", repr, ty),
        }
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
                let op = match &sig.native {
                    Some(sym) => Op::Native { symbol: sym.clone(), args: arg_vs },
                    None => Op::Call { func: mangle(&sig.module, &sig.name), args: arg_vs },
                };
                return self.b.emit(op, repr, ty);
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
                let m = self.env.impls()[impl_id.0].methods.iter().find(|m| m.name == method);
                match m.and_then(|m| m.native.clone()) {
                    Some(sym) => self.b.emit(Op::Native { symbol: sym, args }, repr, ty),
                    None => self.unhandled_note("dispatch to user impl", repr, ty),
                }
            }
            Resolution::Switch(_) => self.unhandled_note("dispatch switch", repr, ty),
            Resolution::Bound { .. } => self.unhandled_note("dispatch bound", repr, ty),
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
