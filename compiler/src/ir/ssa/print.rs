//! The IR's textual form: a canonical dump, LLVM-ish but deliberately its own so it is
//! never mistaken for LLVM. For reading a lowering or a pass, and as golden-test
//! substrate. Printer only — there is no parser, by decision.

use super::{Block, Func, Op, PrimOp, Program, SwitchKey, Target, Term, Value};
use crate::ir::repr::Repr;
use std::fmt::Write;

pub fn program(p: &Program) -> String {
    let mut out = String::new();
    for (i, f) in p.funcs.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&func(f));
    }
    out
}

pub fn func(f: &Func) -> String {
    let mut out = String::new();
    let params: Vec<String> =
        f.params.iter().map(|&p| format!("{} {}", val(p), repr(f.value_repr(p)))).collect();
    let _ = writeln!(out, "fn @{}({}) -> {} {{", f.name, params.join(", "), repr(&f.ret));
    for b in &f.blocks {
        block(&mut out, f, b);
    }
    out.push_str("}\n");
    out
}

fn block(out: &mut String, f: &Func, b: &Block) {
    // The entry block's parameters are the function's, already on the signature; any
    // other block shows its parameters, and a parameterless block shows none.
    if b.id == f.entry || b.params.is_empty() {
        let _ = writeln!(out, "  block{}:", b.id.0);
    } else {
        let params: Vec<String> =
            b.params.iter().map(|&p| format!("{} {}", val(p), repr(f.value_repr(p)))).collect();
        let _ = writeln!(out, "  block{}({}):", b.id.0, params.join(", "));
    }
    for inst in &b.insts {
        match inst.result {
            Some(v) => {
                let _ = writeln!(out, "    {} = {}", val(v), op(&inst.op));
            }
            None => {
                let _ = writeln!(out, "    {}", op(&inst.op));
            }
        }
    }
    let _ = writeln!(out, "    {}", term(&b.term));
}

fn op(o: &Op) -> String {
    match o {
        Op::ConstI64(n) => format!("const.i64 {n}"),
        Op::ConstF64(bits) => format!("const.f64 0x{bits:016x}"),
        Op::ConstBool(b) => format!("const.bool {b}"),
        Op::ConstStr(s) => format!("const.str {s:?}"),
        Op::ConstNull => "const.null".to_string(),
        Op::ConstUnit => "const.unit".to_string(),
        Op::ConstAtom(a) => format!("const.atom :{a}"),
        Op::Prim(p, args) => format!("prim.{} {}", prim(*p), vals(args)),
        Op::Call { func, args } => format!("call @{func}({})", vals(args)),
        Op::Native { symbol, args } => format!("native {symbol:?}({})", vals(args)),
        Op::CallClosure { callee, args } => format!("call.closure {}({})", val(*callee), vals(args)),
        Op::MakeClosure { func, captures } => format!("closure @{func}[{}]", vals(captures)),
        Op::MakeRecord { name, fields } => {
            let fs: Vec<String> = fields.iter().map(|(n, v)| format!("{n}: {}", val(*v))).collect();
            let n = name.as_deref().unwrap_or("");
            format!("record {n}{{{}}}", fs.join(", "))
        }
        Op::Field { base, field } => format!("field {}.{field}", val(*base)),
        Op::MakeTuple(vs) => format!("tuple ({})", vals(vs)),
        Op::Elem { base, index } => format!("elem {}.{index}", val(*base)),
        Op::Cast(v) => format!("cast {}", val(*v)),
        Op::IsErr(v) => format!("is_err {}", val(*v)),
        Op::UnwrapOk(v) => format!("unwrap_ok {}", val(*v)),
        Op::UnwrapErr(v) => format!("unwrap_err {}", val(*v)),
        Op::IsNull(v) => format!("is_null {}", val(*v)),
        Op::IsVariant { value, variant } => format!("is_variant {} {variant}", val(*value)),
        Op::MakeList(vs) => format!("list [{}]", vals(vs)),
        Op::Index { base, index } => format!("index {}[{}]", val(*base), val(*index)),
        Op::Retain(v) => format!("retain {}", val(*v)),
        Op::Release(v) => format!("release {}", val(*v)),
    }
}

fn term(t: &Term) -> String {
    match t {
        Term::Ret(Some(v)) => format!("ret {}", val(*v)),
        Term::Ret(None) => "ret".to_string(),
        Term::Throw(v) => format!("throw {}", val(*v)),
        Term::Jump(tgt) => format!("jump {}", target(tgt)),
        Term::Branch { cond, then, els } => {
            format!("branch {}, {}, {}", val(*cond), target(then), target(els))
        }
        Term::Switch { on, arms, default } => {
            let arms: Vec<String> =
                arms.iter().map(|(k, tgt)| format!("{} => {}", key(k), target(tgt))).collect();
            format!("switch {} [{}] default {}", val(*on), arms.join(", "), target(default))
        }
        Term::Unreachable => "unreachable".to_string(),
    }
}

fn target(t: &Target) -> String {
    if t.args.is_empty() {
        format!("block{}", t.to.0)
    } else {
        format!("block{}({})", t.to.0, vals(&t.args))
    }
}

fn key(k: &SwitchKey) -> String {
    match k {
        SwitchKey::Int(n) => n.to_string(),
        SwitchKey::Bool(b) => b.to_string(),
        SwitchKey::Atom(a) => format!(":{a}"),
        SwitchKey::Nominal(n) => n.clone(),
    }
}

fn val(v: Value) -> String {
    format!("%{}", v.0)
}

fn vals(vs: &[Value]) -> String {
    vs.iter().map(|&v| val(v)).collect::<Vec<_>>().join(", ")
}

fn prim(p: PrimOp) -> &'static str {
    match p {
        PrimOp::Add => "add",
        PrimOp::Sub => "sub",
        PrimOp::Mul => "mul",
        PrimOp::Div => "div",
        PrimOp::Rem => "rem",
        PrimOp::Neg => "neg",
        PrimOp::Eq => "eq",
        PrimOp::Ne => "ne",
        PrimOp::Lt => "lt",
        PrimOp::Le => "le",
        PrimOp::Gt => "gt",
        PrimOp::Ge => "ge",
        PrimOp::And => "and",
        PrimOp::Or => "or",
        PrimOp::Not => "not",
        PrimOp::Band => "band",
        PrimOp::Bor => "bor",
        PrimOp::Bxor => "bxor",
        PrimOp::Bsl => "bsl",
        PrimOp::Bsr => "bsr",
        PrimOp::Bnot => "bnot",
    }
}

/// The textual form of a repr, its own syntax so an IR dump reads at a glance.
pub fn repr(r: &Repr) -> String {
    match r {
        Repr::I64 => "i64".into(),
        Repr::F64 => "f64".into(),
        Repr::Bool => "bool".into(),
        Repr::Str => "str".into(),
        Repr::Null => "null".into(),
        Repr::Unit => "()".into(),
        Repr::Tag => "tag".into(),
        Repr::File => "File".into(),
        Repr::BoxedRec(a) => format!("box#{a}"),
        Repr::Record { name, fields } => {
            let fs: Vec<String> = fields.iter().map(|(n, r)| format!("{n}: {}", repr(r))).collect();
            format!("{}{{{}}}", name.as_deref().unwrap_or(""), fs.join(", "))
        }
        Repr::Tuple(rs) => format!("({})", rs.iter().map(repr).collect::<Vec<_>>().join(", ")),
        Repr::List(e) => format!("list[{}]", repr(e)),
        Repr::Map(k, v) => format!("map[{}, {}]", repr(k), repr(v)),
        Repr::Closure { params, ret } => {
            format!("fn({}) -> {}", params.iter().map(repr).collect::<Vec<_>>().join(", "), repr(ret))
        }
        Repr::Union(vs) => vs.iter().map(repr).collect::<Vec<_>>().join(" | "),
        Repr::Nullable(r) => format!("{}?", repr(r)),
        Repr::Var(n) => format!("'{n}"),
        Repr::Recursive(id) => format!("rec#{}", id.0),
        Repr::Any => "any".into(),
        Repr::Never => "never".into(),
    }
}
