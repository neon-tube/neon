//! The C backend: an IR `Program` becomes one C translation unit, compiled and linked
//! against `runtime/` by `cc`. See `docs/design/ir.md`.
//!
//! Reprs become C types (`i64`→`int64_t`, `str`→`neon_str`, …); SSA values become
//! function-scoped locals; a block becomes a label; a terminator becomes a `goto`,
//! `return`, or `switch`, with block arguments assigned at the edge before the jump —
//! which is how SSA-with-block-arguments lowers to C without φ-nodes.
//!
//! This is a growing emitter: the scalar, string, call and control-flow core is here;
//! aggregates, unions, closures, and the tagged-result calling convention arrive with
//! the runtime that backs them.

use crate::ir::repr::Repr;
use crate::ir::ssa::{Block, Func, Op, PrimOp, Program, SwitchKey, Target, Term, Value};
use std::fmt::Write;

/// Emit the whole program as C source.
pub fn emit(program: &Program) -> String {
    let mut out = String::new();
    out.push_str("#include \"rt.h\"\n\n");

    // Forward declarations, so call order does not matter.
    for f in &program.funcs {
        let _ = writeln!(out, "{};", signature(f));
    }
    out.push('\n');

    for f in &program.funcs {
        emit_fn(&mut out, f);
        out.push('\n');
    }

    // The C entry point, if this program has a `main`.
    if program.funcs.iter().any(|f| f.name == "main") {
        out.push_str(
            "int main(void) {\n    neon_rt_init();\n    nl_main();\n    return 0;\n}\n",
        );
    }
    out
}

/// A function's C signature (no trailing `;` or body).
fn signature(f: &Func) -> String {
    let params: Vec<String> =
        f.params.iter().map(|&p| format!("{} {}", c_type(f.value_repr(p)), var(p))).collect();
    let params = if params.is_empty() { "void".to_string() } else { params.join(", ") };
    format!("{} {}({})", c_ret_type(&f.ret), mangle(&f.name), params)
}

/// The C return type: a unit-returning function is `void`, everything else its value type.
fn c_ret_type(r: &Repr) -> String {
    if matches!(r, Repr::Unit) {
        "void".into()
    } else {
        c_type(r)
    }
}

fn emit_fn(out: &mut String, f: &Func) {
    let _ = writeln!(out, "{} {{", signature(f));

    // Declare every value as a function-scoped local, except the parameters (already in
    // the signature). Assignments below give each its value before use.
    let params: std::collections::HashSet<Value> = f.params.iter().copied().collect();
    for v in f.values() {
        if !params.contains(&v) {
            let _ = writeln!(out, "    {} {};", c_type(f.value_repr(v)), var(v));
        }
    }

    for b in &f.blocks {
        emit_block(out, f, b);
    }
    out.push_str("}\n");
}

fn emit_block(out: &mut String, f: &Func, b: &Block) {
    let _ = writeln!(out, "  block{}:; ", b.id.0);
    for inst in &b.insts {
        let rhs = op_rhs(f, &inst.op);
        match inst.result {
            // A void-typed result (a unit-returning call) is a bare statement.
            Some(v) if !matches!(f.value_repr(v), Repr::Unit) => {
                let _ = writeln!(out, "    {} = {};", var(v), rhs);
            }
            _ => {
                let _ = writeln!(out, "    {};", rhs);
            }
        }
    }
    emit_term(out, f, &b.term);
}

fn emit_term(out: &mut String, f: &Func, term: &Term) {
    match term {
        Term::Ret(Some(v)) if matches!(f.value_repr(*v), Repr::Unit) => {
            out.push_str("    return;\n");
        }
        Term::Ret(Some(v)) => {
            let _ = writeln!(out, "    return {};", var(*v));
        }
        Term::Ret(None) => out.push_str("    return;\n"),
        Term::Throw(v) => {
            // No tagged-result convention yet; a bare throw aborts for now.
            let _ = writeln!(out, "    neon_panic({});", var(*v));
        }
        Term::Jump(t) => emit_jump(out, f, t),
        Term::Branch { cond, then, els } => {
            let _ = writeln!(out, "    if ({}) {{", var(*cond));
            emit_jump(out, f, then);
            out.push_str("    } else {\n");
            emit_jump(out, f, els);
            out.push_str("    }\n");
        }
        Term::Switch { on, arms, default } => {
            let _ = writeln!(out, "    switch ({}) {{", var(*on));
            for (k, t) in arms {
                let _ = writeln!(out, "    case {}: {{", switch_key(k));
                emit_jump(out, f, t);
                out.push_str("    }\n");
            }
            out.push_str("    default: {\n");
            emit_jump(out, f, default);
            out.push_str("    }\n    }\n");
        }
        Term::Unreachable => out.push_str("    neon_unreachable();\n"),
    }
}

/// A jump: assign the target block's parameters from the arguments, then `goto`.
fn emit_jump(out: &mut String, f: &Func, t: &Target) {
    let params = &f.block(t.to).params;
    for (&p, &a) in params.iter().zip(&t.args) {
        if p != a {
            let _ = writeln!(out, "        {} = {};", var(p), var(a));
        }
    }
    let _ = writeln!(out, "        goto block{};", t.to.0);
}

/// The right-hand side C expression (or statement) for an op.
fn op_rhs(f: &Func, op: &Op) -> String {
    match op {
        Op::ConstI64(n) => c_i64(*n),
        Op::ConstF64(bits) => format!("neon_f64_bits({bits}ULL)"),
        Op::ConstBool(b) => b.to_string(),
        Op::ConstStr(s) => format!("neon_str_lit({}, {})", c_string(s), s.len()),
        Op::ConstNull => "neon_null()".into(),
        Op::ConstUnit => "neon_unit_v()".into(),
        Op::ConstAtom(a) => format!("neon_atom({})", atom_hash(a)),
        Op::Prim(p, args) => prim(f, *p, args),
        Op::Call { func, args } => format!("{}({})", mangle(func), vars(args)),
        Op::Native { symbol, args } => format!("{symbol}({})", vars(args)),
        Op::CallClosure { callee, args } => {
            let mut a = vec![format!("{}.env", var(*callee))];
            a.extend(args.iter().map(|&v| var(v)));
            format!("{}.fn({})", var(*callee), a.join(", "))
        }
        Op::Retain(v) => retain_release(f, "neon_retain", *v),
        Op::Release(v) => retain_release(f, "neon_release", *v),
        // Aggregates, unions, and closures are emitted with the runtime that backs them.
        other => format!("/* TODO: {} */ 0", op_name(other)),
    }
}

/// Retain/release act on the object header. For a `str` that is its `owner` field; for a
/// pointer-backed value the value itself is the header.
fn retain_release(f: &Func, func: &str, v: Value) -> String {
    match f.value_repr(v) {
        Repr::Str => format!("{func}({}.owner)", var(v)),
        _ => format!("{func}((neon_header*){})", var(v)),
    }
}

fn prim(f: &Func, op: PrimOp, args: &[Value]) -> String {
    let is_float = args.first().is_some_and(|&v| matches!(f.value_repr(v), Repr::F64));
    let is_str = args.first().is_some_and(|&v| matches!(f.value_repr(v), Repr::Str));
    let a = args.first().map(|&v| var(v)).unwrap_or_default();
    let b = args.get(1).map(|&v| var(v)).unwrap_or_default();
    match op {
        // i64 arithmetic traps on overflow; f64 is plain.
        PrimOp::Add if is_float => format!("({a} + {b})"),
        PrimOp::Add => format!("neon_i64_add({a}, {b})"),
        PrimOp::Sub if is_float => format!("({a} - {b})"),
        PrimOp::Sub => format!("neon_i64_sub({a}, {b})"),
        PrimOp::Mul if is_float => format!("({a} * {b})"),
        PrimOp::Mul => format!("neon_i64_mul({a}, {b})"),
        PrimOp::Div if is_float => format!("({a} / {b})"),
        PrimOp::Div => format!("neon_i64_div({a}, {b})"),
        PrimOp::Rem => format!("neon_i64_rem({a}, {b})"),
        PrimOp::Neg if is_float => format!("(-{a})"),
        PrimOp::Neg => format!("neon_i64_neg({a})"),
        PrimOp::Eq if is_str => format!("neon_str_eq({a}, {b})"),
        PrimOp::Ne if is_str => format!("(!neon_str_eq({a}, {b}))"),
        PrimOp::Eq => format!("({a} == {b})"),
        PrimOp::Ne => format!("({a} != {b})"),
        PrimOp::Lt => format!("({a} < {b})"),
        PrimOp::Le => format!("({a} <= {b})"),
        PrimOp::Gt => format!("({a} > {b})"),
        PrimOp::Ge => format!("({a} >= {b})"),
        PrimOp::And => format!("({a} && {b})"),
        PrimOp::Or => format!("({a} || {b})"),
        PrimOp::Not => format!("(!{a})"),
        PrimOp::Band => format!("({a} & {b})"),
        PrimOp::Bor => format!("({a} | {b})"),
        PrimOp::Bxor => format!("({a} ^ {b})"),
        PrimOp::Bsl => format!("({a} << {b})"),
        PrimOp::Bsr => format!("({a} >> {b})"),
        PrimOp::Bnot => format!("(~{a})"),
    }
}

// ---- C types and names ----

fn c_type(r: &Repr) -> String {
    match r {
        Repr::I64 => "int64_t".into(),
        Repr::F64 => "double".into(),
        Repr::Bool => "bool".into(),
        Repr::Str => "neon_str".into(),
        Repr::Unit | Repr::Null => "neon_unit".into(),
        Repr::Tag => "uint64_t".into(),
        Repr::List(_) => "neon_list*".into(),
        Repr::Map(_, _) => "neon_map*".into(),
        Repr::Closure { .. } => "neon_closure".into(),
        // Aggregates, unions, nullables get named structs once those land; for now a
        // generic boxed value keeps the emitter total.
        _ => "neon_value".into(),
    }
}

/// `nl_` + an injective escape of the IR name into a valid C identifier. Runtime symbols
/// (`neon_*`) are never mangled.
fn mangle(name: &str) -> String {
    let mut out = String::from("nl_");
    for c in name.chars() {
        match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' => out.push(c),
            '$' => out.push_str("_S"),
            '_' => out.push_str("_u"),
            other => {
                let _ = write!(out, "_x{:02x}", other as u32);
            }
        }
    }
    out
}

fn var(v: Value) -> String {
    format!("_{}", v.0)
}

fn vars(vs: &[Value]) -> String {
    vs.iter().map(|&v| var(v)).collect::<Vec<_>>().join(", ")
}

/// `INT64_MIN` cannot be written as a literal; build it from `-INT64_MAX - 1`.
fn c_i64(n: i64) -> String {
    if n == i64::MIN {
        "(-9223372036854775807LL - 1)".into()
    } else {
        format!("{n}LL")
    }
}

fn switch_key(k: &SwitchKey) -> String {
    match k {
        SwitchKey::Int(n) => c_i64(*n),
        SwitchKey::Bool(b) => (*b as i64).to_string(),
        SwitchKey::Atom(a) => atom_hash(a),
        SwitchKey::Nominal(_) => "0".into(), // dense union tag assignment is future work
    }
}

/// A C string literal with the bytes escaped.
fn c_string(s: &str) -> String {
    let mut out = String::from("\"");
    for b in s.bytes() {
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\t' => out.push_str("\\t"),
            b'\r' => out.push_str("\\r"),
            0x20..=0x7e => out.push(b as char),
            _ => {
                let _ = write!(out, "\\x{b:02x}");
            }
        }
    }
    out.push('"');
    out
}

/// FNV-1a of the atom name, matching the runtime's atom-tag scheme.
fn atom_hash(name: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in name.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h}ULL")
}

fn op_name(op: &Op) -> &'static str {
    match op {
        Op::MakeRecord { .. } => "record",
        Op::Field { .. } => "field",
        Op::MakeTuple(_) => "tuple",
        Op::Elem { .. } => "elem",
        Op::MakeList(_) => "list",
        Op::Index { .. } => "index",
        Op::MakeClosure { .. } => "closure",
        Op::Cast(_) => "cast",
        Op::IsNull(_) => "is_null",
        Op::IsVariant { .. } => "is_variant",
        Op::IsErr(_) => "is_err",
        Op::UnwrapOk(_) => "unwrap_ok",
        Op::UnwrapErr(_) => "unwrap_err",
        _ => "op",
    }
}

#[cfg(test)]
mod tests;
