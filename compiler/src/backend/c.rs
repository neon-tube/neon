//! The C backend: an IR `Program` becomes one C translation unit, compiled and linked
//! against `runtime/` by `cc`. See `docs/design/ir.md`.
//!
//! Reprs become C types (`i64`→`int64_t`, `str`→`neon_str`, a record→a named `struct`, …);
//! SSA values become function-scoped locals; a block becomes a label; a terminator becomes
//! a `goto`, `return`, or `switch`, with block arguments assigned at the edge before the
//! jump — which is how SSA-with-block-arguments lowers to C without φ-nodes.
//!
//! This is a growing emitter: scalars, strings, calls, control flow, and inline aggregates
//! (records and tuples) are here; unions, the tagged-result calling convention, and the
//! container runtime arrive with the pieces that back them.

use crate::backend::ctype::{field_name, fnv1a, type_tag, TypeTable};
use crate::ir::repr::Repr;
use crate::ir::ssa::{Block, Func, Op, PrimOp, Program, SwitchKey, Target, Term, Value};
use std::fmt::Write;

/// Emit the whole program as C source.
pub fn emit(program: &Program) -> String {
    let types = TypeTable::build(program);
    let mut out = String::new();
    out.push_str("#include \"rt.h\"\n\n");

    // Aggregate struct definitions, before any function that uses them.
    types.emit_defs(&mut out);
    // Before the witnesses: an element witness for a boxed record calls into these.
    emit_boxed_eq(&mut out, &types);
    emit_witnesses(&mut out, &types);
    emit_key_witnesses(&mut out, &types);
    emit_env_drops(&mut out, &types);

    // Forward declarations, so call order does not matter.
    for f in &program.funcs {
        let _ = writeln!(out, "{};", signature(&types, f));
    }
    out.push('\n');

    // Adapter thunks give ordinary functions used as closure values the closure ABI.
    emit_thunks(&mut out, &types, program);
    emit_resource_drops(&mut out, &types, program);

    for f in &program.funcs {
        emit_fn(&mut out, &types, f);
        out.push('\n');
    }

    // The C entry point, if this program has a `main`.
    if program.funcs.iter().any(|f| f.name == "main") {
        out.push_str(
            "int main(void) {\n    neon_rt_init();\n    nl_main();\n    return 0;\n}\n",
        );
    }
    reindent(&out)
}

/// Lay out the finished source. Indentation is *derived* here from brace nesting rather
/// than hand-counted at each `writeln!` — `cc` ignores whitespace entirely, so the only
/// reason it exists is for reading the generated `.c` while debugging codegen, and one
/// pass that understands nesting beats spaces baked into hundreds of format strings.
fn reindent(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut depth: usize = 0;
    for raw in src.lines() {
        let line = raw.trim();
        if line.is_empty() {
            out.push('\n');
            continue;
        }
        // A line that closes a block dedents itself; a label sits one level out.
        let closes = line.starts_with('}');
        let is_label = line.ends_with(':') || line.ends_with(":;");
        let mut indent = depth;
        if closes || is_label {
            indent = indent.saturating_sub(1);
        }
        for _ in 0..indent {
            out.push_str("    ");
        }
        out.push_str(line);
        out.push('\n');
        depth = (depth as i32 + net_braces(line)).max(0) as usize;
    }
    out
}

/// The net `{` minus `}` on a line, ignoring braces inside string and character literals
/// (a `neon_str_lit("{", 1)` must not open a block).
fn net_braces(line: &str) -> i32 {
    let mut net = 0;
    let mut chars = line.chars();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == '\\' {
                    chars.next(); // skip the escaped character
                } else if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(c),
                '{' => net += 1,
                '}' => net -= 1,
                _ => {}
            },
        }
    }
    net
}

/// A function's C signature (no trailing `;` or body). A lifted lambda's first parameter
/// is its boxed environment, received as a `neon_header*`.
fn signature(types: &TypeTable, f: &Func) -> String {
    let params: Vec<String> = f
        .params
        .iter()
        .enumerate()
        .map(|(i, &p)| {
            if i == 0 && f.env.is_some() {
                "neon_header* _env".to_string()
            } else {
                format!("{} {}", types.c_type(f.value_repr(p)), var(p))
            }
        })
        .collect();
    let params = if params.is_empty() { "void".to_string() } else { params.join(", ") };
    format!("{} {}({})", fn_ret_type(types, f), mangle(&f.name), params)
}

/// A function's C return type. A throwing function returns its tagged result rather than
/// its declared type — that is the whole calling convention.
fn fn_ret_type(types: &TypeTable, f: &Func) -> String {
    match f.result_repr() {
        Some(res) => types.c_type(&res),
        None => c_ret_type(types, &f.ret),
    }
}

/// The C return type: a unit-returning function is `void`, everything else its value type.
fn c_ret_type(types: &TypeTable, r: &Repr) -> String {
    if matches!(r, Repr::Unit) {
        "void".into()
    } else {
        types.c_type(r)
    }
}

fn emit_fn(out: &mut String, types: &TypeTable, f: &Func) {
    let _ = writeln!(out, "{} {{", signature(types, f));

    // Declare every value as a function-scoped local, except the parameters (already in
    // the signature). Assignments below give each its value before use.
    let params: std::collections::HashSet<Value> = f.params.iter().copied().collect();
    for v in f.values() {
        if !params.contains(&v) {
            let _ = writeln!(out, "{} {};", types.c_type(f.value_repr(v)), var(v));
        }
    }

    // A lambda with captures unpacks its boxed environment into the tuple its body reads.
    if let Some(env) = &f.env {
        if matches!(env, Repr::Tuple(fields) if !fields.is_empty()) {
            let ev = f.params[0];
            let ty = types.c_type(env);
            let _ = writeln!(out, "{ty} {} = *({ty}*)(_env + 1);", var(ev));
        }
    }

    for b in &f.blocks {
        emit_block(out, types, f, b);
    }
    out.push_str("}\n");
}

/// The values in a function that hold a throwing call's tagged result.
fn throwing_call_results(
    types: &TypeTable,
    f: &Func,
) -> std::collections::HashMap<Value, Repr> {
    let mut out = std::collections::HashMap::new();
    for b in &f.blocks {
        for inst in &b.insts {
            if let (Some(v), Op::Call { func, .. }) = (inst.result, &inst.op) {
                if let Some(r) = types.result_of(func) {
                    out.insert(v, r.clone());
                }
            }
        }
    }
    out
}

fn emit_block(out: &mut String, types: &TypeTable, f: &Func, b: &Block) {
    let _ = writeln!(out, "block{}:; ", b.id.0);
    for inst in &b.insts {
        emit_inst(out, types, f, inst);
    }
    emit_term(out, types, f, &b.term);
}

/// Emit one instruction. Most ops are a single expression assigned to their result; the
/// container ops need a short statement sequence (a bounds check, a witness, a slot store),
/// so those are handled here before falling back to [`op_rhs`].
fn emit_inst(out: &mut String, types: &TypeTable, f: &Func, inst: &crate::ir::ssa::Inst) {
    match &inst.op {
        Op::MakeList(elems) => emit_make_list(out, types, f, inst.result, elems),
        // A recursive record is heap-allocated, which is two statements: claim the memory,
        // then move the built value in. `neon_alloc` prepends the header and returns it,
        // and the wrapper carries that header first, so the pointer needs no adjusting.
        Op::MakeRecord { .. }
            if inst.result.is_some_and(|v| types.is_boxed(f.value_repr(v))) =>
        {
            let r = inst.result.unwrap();
            let (wrapper, shape) = types.boxed_shape(f.value_repr(r)).unwrap();
            let sty = types.c_type(shape);
            let _ = writeln!(
                out,
                "{} = ({wrapper}*)neon_alloc(sizeof({sty}), {});",
                var(r),
                types.env_drop_ref(shape)
            );
            let _ = writeln!(out, "{}->value = {};", var(r), op_rhs(types, f, inst.result, &inst.op));
        }
        Op::Index { base, index } => emit_index(out, types, f, inst.result, *base, *index),
        Op::Native { symbol, args } if is_list_builder(symbol) => {
            emit_list_builder(out, types, f, inst.result, symbol, args)
        }
        // A native whose Neon signature returns a tuple takes the extra slots as C
        // out-parameters. See `emit_native_out`.
        Op::Native { symbol, args }
            if inst.result.is_some_and(|v| matches!(f.value_repr(v), Repr::Tuple(_))) =>
        {
            emit_native_out(out, types, f, inst.result, symbol, args)
        }
        Op::MakeClosure { func, captures } => {
            emit_make_closure(out, types, f, inst.result, func, captures)
        }
        _ => {
            let rhs = op_rhs(types, f, inst.result, &inst.op);
            match inst.result {
                // A void-typed result (a unit-returning call) is a bare statement.
                Some(v) if !matches!(f.value_repr(v), Repr::Unit) => {
                    let _ = writeln!(out, "{} = {};", var(v), rhs);
                }
                _ => {
                    let _ = writeln!(out, "{};", rhs);
                }
            }
        }
    }
}

/// The element repr of a `List` result value.
/// A value's list element repr, looking through a union it may be injected into.
fn list_elem(types: &TypeTable, f: &Func, v: Value) -> Repr {
    match list_variant(types, f.value_repr(v)) {
        Some(Repr::List(e)) => *e,
        _ => Repr::Any,
    }
}

/// The `List` a value is, or the `List` variant it is injected into: `let xs: A = [..]`
/// where `A` is a union types the literal as the union, but what is *built* is still a
/// list, and it is injected on the way out.
fn list_variant(types: &TypeTable, r: &Repr) -> Option<Repr> {
    match types.resolve(r) {
        l @ Repr::List(_) => Some(l.clone()),
        Repr::Union(vs) => vs.iter().find_map(|v| list_variant(types, v)),
        Repr::Nullable(inner) => list_variant(types, inner),
        _ => None,
    }
}

/// `[a, b, c]` — allocate a list sized for the elements, then move each into its slot. The
/// elements are consumed (ownership moves in), so no retain.
fn emit_make_list(out: &mut String, types: &TypeTable, f: &Func, result: Option<Value>, elems: &[Value]) {
    let Some(r) = result else { return };
    let target = types.resolve(f.value_repr(r)).clone();
    // What is built is always a list; the value's own repr may be a union it injects into.
    let list = list_variant(types, &target).unwrap_or(Repr::List(Box::new(Repr::Any)));
    let elem = match &list {
        Repr::List(e) => (**e).clone(),
        _ => Repr::Any,
    };
    let ety = types.c_type(&elem);
    let n = elems.len();
    let direct = target == list;
    let dest = if direct { var(r) } else { format!("{}_l", var(r)) };
    if !direct {
        let _ = writeln!(out, "neon_list* {dest};");
    }
    let _ = writeln!(out, "{dest} = neon_list_new_with_capacity({}, {n});", types.witness_ref(&elem));
    // Each element is coerced into the list's element repr, so a concrete value flowing
    // into a `List[i64 | null]` or a covariant `List[Shape]` is injected on the way in.
    for (i, &v) in elems.iter().enumerate() {
        let val = coerce(types, f, v, &elem);
        let _ = writeln!(out, "(({ety}*){dest}->data)[{i}] = {val};");
    }
    let _ = writeln!(out, "{dest}->len = {n};");
    if !direct {
        let _ = writeln!(out, "{} = {};", var(r), coerce_expr(types, &dest, &list, &target));
    }
}

/// `xs[i]` — bounds-checked read of an element (traps on a bad index), retaining it so the
/// caller owns its own reference.
fn emit_index(out: &mut String, types: &TypeTable, f: &Func, result: Option<Value>, base: Value, index: Value) {
    let Some(r) = result else { return };
    let elem = f.value_repr(r).clone();
    let ety = types.c_type(&elem);
    // `m[k]` looks the key up by address and traps when it is absent; `xs[i]` indexes.
    if matches!(f.value_repr(base), Repr::Map(_, _)) {
        let _ = writeln!(out, "{} = *({ety}*)neon_map_at({}, &{});", var(r), var(base), var(index));
    } else {
        let _ = writeln!(out, "{} = *({ety}*)neon_list_at({}, {});", var(r), var(base), var(index));
    }
    let mut parts = Vec::new();
    rc_parts(types, "neon_retain", &elem, &var(r), &mut parts);
    if !parts.is_empty() {
        let _ = writeln!(out, "{};", parts.join(", "));
    }
}

/// The list natives whose element crosses the ABI boundary (a witness for construction, a
/// slot pointer for insertion) and so cannot use the plain by-value native call.
fn is_list_builder(symbol: &str) -> bool {
    matches!(
        symbol,
        "neon_list_new"
            | "neon_list_new_with_capacity"
            | "neon_list_push"
            | "neon_list_set"
            | "neon_map_new"
            | "neon_map_set"
            | "neon_map_contains"
            | "neon_map_remove"
            | "neon_map_keys"
            | "neon_map_values"
            | "neon_resource_new"
    )
}

/// The cleanup closure's shape: its payload parameter, what it throws, and what it
/// returns.
///
/// Read off the *closure argument* rather than reconstructed from the resource's type.
/// The closure repr already states all three, and it is the thing the emitted drop
/// actually calls — so there is no name to match on, no argument position to assume, and
/// no second place that has to agree with `std::resource`'s signature. Deriving it from
/// `Repr::Runtime { name: "neon_resource", args }` was exactly the hardcoded name lookup
/// that `@runtime` exists to delete.
fn cleanup_shape(f: &Func, cleanup: Value) -> Option<(Repr, Repr, Repr)> {
    match f.value_repr(cleanup) {
        Repr::Closure { params, throws, ret } if params.len() == 1 => {
            Some((params[0].clone(), (**throws).clone(), (**ret).clone()))
        }
        _ => None,
    }
}

/// The name of the drop emitted for one cleanup shape.
fn resource_drop_name(types: &TypeTable, t: &Repr, e: &Repr) -> String {
    format!("nres_drop_{}", types.witness_ref(t).trim_start_matches('&'))
        + &format!("_{}", types.witness_ref(e).trim_start_matches('&'))
}

/// The key and value reprs of a `Map` value.
fn map_kv(f: &Func, v: Value) -> (Repr, Repr) {
    match f.value_repr(v) {
        Repr::Map(k, val) => ((**k).clone(), (**val).clone()),
        _ => (Repr::Any, Repr::Any),
    }
}

/// A native that returns several values.
///
/// A C function returns one value, and a native can build neither a record nor a tuple --
/// codegen owns those layouts, and they differ per program. So an operation that produces
/// data *and* can fail had nowhere to put the status, which is what pushed an earlier
/// draft of `std::fs` into an errno-style global.
///
/// The fix is a calling convention rather than a language feature. A `@native` whose Neon
/// return type is a tuple takes the tail of that tuple as C out-parameters:
///
///     @native("neon_io_read_all") fn read_all(fd: i64) -> (str, i64)
///     // calls: neon_str neon_io_read_all(int64_t fd, int64_t* out_1)
///
/// Nothing new appears in the language: the caller sees an ordinary tuple and destructures
/// it. No annotation is needed either, and that is not a heuristic -- a native can never
/// return a tuple *by value*, since it cannot name the generated struct, so a tuple return
/// on a native means out-parameters and nothing else.
fn emit_native_out(
    out: &mut String,
    types: &TypeTable,
    f: &Func,
    result: Option<Value>,
    symbol: &str,
    args: &[Value],
) {
    let Some(r) = result else { return };
    let Repr::Tuple(elems) = f.value_repr(r).clone() else { return };
    let Some((first, rest)) = elems.split_first() else { return };

    let mut call_args: Vec<String> = args.iter().map(|&v| prim_operand(f, v)).collect();
    let slot = |i: usize| format!("{}_out{i}", var(r));

    let _ = writeln!(out, "{{");
    for (i, e) in rest.iter().enumerate() {
        let _ = writeln!(out, "{} {};", types.c_type(e), slot(i));
        call_args.push(format!("&{}", slot(i)));
    }
    let call = format!("{symbol}({})", call_args.join(", "));

    // The direct return is the tuple's first element; a `()` there means the native
    // returns nothing and every result travels through an out-parameter.
    let mut fields: Vec<String> = Vec::new();
    if matches!(first, Repr::Unit) {
        let _ = writeln!(out, "{call};");
        fields.push("._0 = neon_unit_v()".to_string());
    } else {
        let _ = writeln!(out, "{} {}_ret = {call};", types.c_type(first), var(r));
        fields.push(format!("._0 = {}_ret", var(r)));
    }
    for i in 0..rest.len() {
        fields.push(format!("._{} = {}", i + 1, slot(i)));
    }
    let _ = writeln!(out, "{} = ({}){{ {} }};", var(r), types.c_type(f.value_repr(r)), fields.join(", "));
    let _ = writeln!(out, "}}");
}

fn emit_list_builder(out: &mut String, types: &TypeTable, f: &Func, result: Option<Value>, symbol: &str, args: &[Value]) {
    let Some(r) = result else { return };
    let elem = list_elem(types, f, r);
    let w = types.witness_ref(&elem);
    let rhs = match symbol {
        // A map's key crosses the boundary by address, like a list element, and its
        // witnesses come from the emitter — the runtime cannot know them.
        "neon_map_new" => {
            let (k, v) = map_kv(f, r);
            format!("neon_map_new({}, {})", types.key_witness_ref(&k), types.witness_ref(&v))
        }
        "neon_map_set" => {
            let (k, v) = map_kv(f, r);
            format!(
                "neon_map_set({}, {}, {})",
                var(args[0]),
                addr_of(types, f, args[1], &k),
                addr_of(types, f, args[2], &v)
            )
        }
        "neon_map_contains" => format!("neon_map_contains({}, &{})", var(args[0]), var(args[1])),
        "neon_map_remove" => {
            // The key crosses by address and must be coerced to the map's key repr first,
            // exactly as `set` does -- a narrower argument would otherwise be read at the
            // wrong width through the void pointer.
            let (k, _) = map_kv(f, r);
            format!("neon_map_remove({}, {})", var(args[0]), addr_of(types, f, args[1], &k))
        }
        "neon_map_keys" | "neon_map_values" => {
            let elem = list_elem(types, f, r);
            format!("{symbol}({}, {})", var(args[0]), types.witness_ref(&elem))
        }
        // The payload crosses by address (the resource memcpy's it in through the
        // witness), the cleanup closure goes by value, and the drop is this
        // instantiation's own -- the only code that knows how to call the closure.
        "neon_resource_new" => {
            let (t, e, _) = cleanup_shape(f, args[1]).expect("cleanup is a one-arg closure");
            format!(
                "neon_resource_new({}, {}, {}, {})",
                addr_of(types, f, args[0], &t),
                types.witness_ref(&t),
                var(args[1]),
                resource_drop_name(types, &t, &e)
            )
        }
        "neon_list_new" => format!("neon_list_new({w})"),
        "neon_list_new_with_capacity" => format!("neon_list_new_with_capacity({w}, {})", var(args[0])),
        // The element is passed by address; the list moves its bytes in through the witness.
        "neon_list_push" => {
            format!("neon_list_push({}, {})", var(args[0]), addr_of(types, f, args[1], &elem))
        }
        "neon_list_set" => format!(
            "neon_list_set({}, {}, {})",
            var(args[0]),
            var(args[1]),
            addr_of(types, f, args[2], &elem)
        ),
        _ => unreachable!(),
    };
    let _ = writeln!(out, "{} = {};", var(r), rhs);
}

/// The adapter-thunk name for an ordinary function used as a closure value.
fn thunk_name(func: &str) -> String {
    format!("{}_thunk", mangle(func))
}

/// Emit one drop per `Resource` instantiation.
///
/// The runtime reaches this through `header.drop`, arriving with only a `neon_header*`,
/// so this is the only place that still knows `T` and `E`. It loads the payload at its
/// real type, calls the cleanup closure through a pointer cast to the closure ABI, and
/// releases the tagged result -- an error the drop path cannot report, but must not leak.
///
/// Forgetting that release would leak on the *automatic* path only, invisible to the
/// explicit one, which is exactly the shape of bug that ships.
fn emit_resource_drops(out: &mut String, types: &TypeTable, program: &Program) {
    let mut seen: std::collections::BTreeMap<String, (Repr, Repr, Repr)> =
        std::collections::BTreeMap::new();
    for f in &program.funcs {
        for b in &f.blocks {
            for inst in &b.insts {
                let Op::Native { symbol, .. } = &inst.op else { continue };
                if symbol != "neon_resource_new" {
                    continue;
                }
                let Op::Native { args, .. } = &inst.op else { continue };
                let Some((t, e, ret)) = cleanup_shape(f, args[1]) else { continue };
                seen.insert(resource_drop_name(types, &t, &e), (t, e, ret));
            }
        }
    }
    if seen.is_empty() {
        return;
    }
    for (name, (t, e, ret)) in seen {
        let tc = types.c_type(&t);
        // The tagged result the closure actually returns, built from its own `ret` and
        // `throws` — the same union `Func::result_repr` builds for a throwing function.
        let tagged = Repr::Union(vec![ret, e.clone()]);
        let throws = !matches!(e, Repr::Never);
        let retc = if throws { types.c_type(&tagged) } else { "void".to_string() };
        let _ = writeln!(out, "static void {name}(void* p) {{");
        let _ = writeln!(out, "    neon_resource* r = (neon_resource*)p;");
        let _ = writeln!(out, "    if (r->armed) {{");
        let _ = writeln!(out, "        r->armed = false;");
        let _ = writeln!(out, "        {tc} pay;");
        let _ = writeln!(out, "        memcpy(&pay, neon_resource_payload(r), sizeof pay);");
        let call = format!(
            "(({retc}(*)(neon_header*, {tc}))r->cleanup.fn)(r->cleanup.env, pay)"
        );
        if throws {
            let w = types.witness_ref(&tagged);
            let _ = writeln!(out, "        {retc} res = {call};");
            let _ = writeln!(out, "        const neon_witness* rw = {w};");
            let _ = writeln!(out, "        if (rw && rw->release) rw->release(&res);");
        } else {
            let _ = writeln!(out, "        {call};");
        }
        let _ = writeln!(out, "    }}");
        let _ = writeln!(out, "    neon_resource_finish(r);");
        let _ = writeln!(out, "}}");
    }
    out.push('\n');
}

/// Emit an adapter thunk for every ordinary function used as a closure value: it takes the
/// closure ABI's leading (ignored) environment, then forwards to the real function.
fn emit_thunks(out: &mut String, types: &TypeTable, program: &Program) {
    let by_name: std::collections::HashMap<&str, &Func> =
        program.funcs.iter().map(|f| (f.name.as_str(), f)).collect();
    let mut targets: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for f in &program.funcs {
        for b in &f.blocks {
            for inst in &b.insts {
                if let Op::MakeClosure { func, .. } = &inst.op {
                    if !types.is_lambda(func) {
                        targets.insert(func);
                    }
                }
            }
        }
    }
    if targets.is_empty() {
        return;
    }
    for name in targets {
        let Some(target) = by_name.get(name) else { continue };
        let params: Vec<String> = target
            .params
            .iter()
            .enumerate()
            .map(|(i, &p)| format!("{} _a{i}", types.c_type(target.value_repr(p))))
            .collect();
        let args: Vec<String> = (0..target.params.len()).map(|i| format!("_a{i}")).collect();
        let sig_params = std::iter::once("neon_header* _env".to_string()).chain(params).collect::<Vec<_>>().join(", ");
        let call = format!("{}({})", mangle(name), args.join(", "));
        // A *throwing* function returns its tagged result, not its declared type -- the
        // whole calling convention. Building the thunk from `target.ret` typed the
        // adapter as returning `int64_t` while the call handed back an `nu1`, which the C
        // compiler rejected; `fn_ret_type` is the function that already knows this.
        let ret = fn_ret_type(types, target);
        let returns_void = target.result_repr().is_none() && matches!(target.ret, Repr::Unit);
        let body = if returns_void {
            format!("(void)_env; {call};")
        } else {
            format!("(void)_env; return {call};")
        };
        let _ = writeln!(out, "{ret} {}({sig_params}) {{ {body} }}", thunk_name(name));
    }
    out.push('\n');
}

/// `(args) => body` captured as a closure: a function pointer plus a boxed environment
/// holding the captures (or a null environment when it captures nothing).
fn emit_make_closure(out: &mut String, types: &TypeTable, f: &Func, result: Option<Value>, func: &str, captures: &[Value]) {
    let Some(r) = result else { return };
    // A lambda already has the `(env, args…)` shape; an ordinary function used as a value
    // is reached through its adapter thunk.
    let target = if types.is_lambda(func) { mangle(func) } else { thunk_name(func) };
    let fnptr = format!("(void*){target}");
    if captures.is_empty() {
        let _ = writeln!(out, "{} = (neon_closure){{ {fnptr}, (neon_header*)0 }};", var(r));
        return;
    }
    let env = Repr::Tuple(captures.iter().map(|&c| f.value_repr(c).clone()).collect());
    let ty = types.c_type(&env);
    let drop = types.env_drop_ref(&env);
    let inits: Vec<String> =
        captures.iter().enumerate().map(|(i, &c)| format!("._{i} = {}", var(c))).collect();
    // Move the captures into a fresh heap environment (they are consumed here), then pair
    // it with the function pointer.
    let _ = writeln!(out, "{{ neon_header* _e = neon_alloc(sizeof({ty}), {drop}); *({ty}*)(_e + 1) = ({ty}){{{}}}; {} = (neon_closure){{ {fnptr}, _e }}; }}",
        inits.join(", "),
        var(r),
    );
}

/// Emit a drop function for each closure environment: release its captured references,
/// then free the box.
fn emit_env_drops(out: &mut String, types: &TypeTable) {
    for (name, repr) in types.env_drops() {
        let ty = types.c_type(repr);
        let mut parts = Vec::new();
        rc_parts(types, "neon_release", repr, "(*e)", &mut parts);
        let releases = if parts.is_empty() { String::new() } else { format!("{}; ", parts.join("; ")) };
        let _ = writeln!(
            out,
            "static void {name}(void* p) {{ neon_header* h = (neon_header*)p; {ty}* e = ({ty}*)(h + 1); {releases}neon_free(h); }}",
        );
    }
    if !types.env_drops().is_empty() {
        out.push('\n');
    }
}

/// Emit a key-witness — hash and content equality — for each type used as a map key.
fn emit_key_witnesses(out: &mut String, types: &TypeTable) {
    for (name, repr) in types.key_witnesses() {
        let ty = types.c_type(repr);
        let _ = writeln!(
            out,
            "static uint64_t {name}_hash(const void* p) {{ const {ty}* e = (const {ty}*)p; return {}; }}",
            hash_expr(repr, "(*e)"),
        );
        let _ = writeln!(
            out,
            "static bool {name}_eq(const void* pa, const void* pb) {{ const {ty}* a = (const {ty}*)pa; const {ty}* b = (const {ty}*)pb; return {}; }}",
            eq_expr(types, repr, "(*a)", "(*b)"),
        );
        let _ = writeln!(
            out,
            "static const neon_key_witness {name} = {{ {}, {name}_hash, {name}_eq }};",
            types.witness_ref(repr),
        );
    }
    if !types.key_witnesses().is_empty() {
        out.push('\n');
    }
}

/// Hash a key by *content*. A string hashes its bytes, an aggregate mixes its fields, and
/// anything flat hashes its representation.
fn hash_expr(r: &Repr, e: &str) -> String {
    match r {
        Repr::Str => format!("neon_hash_bytes({e}.data, {e}.len)"),
        Repr::Record { fields, .. } => fields
            .iter()
            .map(|(n, fr)| hash_expr(fr, &format!("{e}.{}", field_name(n))))
            .reduce(|a, b| format!("neon_hash_mix({a}, {b})"))
            .unwrap_or_else(|| "0".into()),
        Repr::Tuple(elems) => elems
            .iter()
            .enumerate()
            .map(|(i, er)| hash_expr(er, &format!("{e}._{i}")))
            .reduce(|a, b| format!("neon_hash_mix({a}, {b})"))
            .unwrap_or_else(|| "0".into()),
        // By length, not by address: `eq_expr` compares lists elementwise, and equal keys
        // must hash equal. Hashing the elements too would need a loop, so a per-element
        // hash on the value-witness -- the pointer the layering deliberately keeps off it.
        // Length alone is a correct hash, just a weak one: `Map[List[T], V]` keyed on
        // same-length lists degrades toward a linear probe, and buying more than that
        // means giving every element type a hash function.
        Repr::List(_) => format!("neon_hash_bytes(&{e}->len, sizeof {e}->len)"),
        _ => format!("neon_hash_bytes(&{e}, sizeof {e})"),
    }
}

/// Structural order, as an `int` expression that is negative, zero or positive — the
/// `memcmp` convention. Aggregates compare lexicographically: a record by field in
/// declaration order, a tuple by position, so the first field that differs decides.
///
/// This is a nested expression rather than a generated function because both operands are
/// already plain lvalues (`prim_operand` hands back a variable or a literal), so repeating
/// one costs nothing and evaluates nothing twice.
///
/// `f64` is compared with `<`/`>` and so is *not* a total order: NaN is neither, and both
/// arms fall through to `0`, reporting NaN equal to everything. That is the documented
/// consequence of using IEEE semantics at the leaf — see "Comparison is structural" in
/// docs/decisions.md — and it is why `sort` on a list holding NaN has no defined result.
/// Whether `cmp_expr` can order this repr — the backend's half of the checker's
/// `is_ordered`. A union has no order (ranking its arms would be an invention), and a
/// closure, map or boxed recursive record has none either; an aggregate is ordered exactly
/// when every part of it is.
fn has_order(r: &Repr) -> bool {
    match r {
        Repr::I64 | Repr::F64 | Repr::Bool | Repr::Tag | Repr::Str | Repr::Unit | Repr::Null => true,
        Repr::Record { fields, .. } => fields.iter().all(|(_, fr)| has_order(fr)),
        Repr::Tuple(elems) => elems.iter().all(has_order),
        Repr::List(e) => has_order(e),
        _ => false,
    }
}

fn cmp_expr(types: &TypeTable, r: &Repr, a: &str, b: &str) -> String {
    match r {
        Repr::Str => format!("neon_str_cmp({a}, {b})"),
        Repr::Record { fields, .. } => fields
            .iter()
            .map(|(n, fr)| {
                let f = field_name(n);
                (fr, format!("{a}.{f}"), format!("{b}.{f}"))
            })
            .rev()
            .fold("0".to_string(), |rest, (fr, fa, fb)| lex_then(types, fr, &fa, &fb, &rest)),
        Repr::Tuple(elems) => elems
            .iter()
            .enumerate()
            .map(|(i, er)| (er, format!("{a}._{i}"), format!("{b}._{i}")))
            .rev()
            .fold("0".to_string(), |rest, (er, ea, eb)| lex_then(types, er, &ea, &eb, &rest)),
        // A list walks its elements through its witness, so one runtime function covers
        // every element type.
        Repr::List(_) => format!("neon_list_cmp({a}, {b})"),
        // One inhabitant, so two of them are always equal. They are `neon_unit` structs in
        // C, which `<` would reject anyway.
        Repr::Unit | Repr::Null => "0".to_string(),
        // Scalars: the standard branchless three-way compare.
        Repr::I64 | Repr::F64 | Repr::Bool | Repr::Tag => format!("(({a} > {b}) - ({a} < {b}))"),
        // Anything else has no structural order, and the checker rejects ordering it
        // (`is_ordered` in typecheck/check.rs). Reaching here means the two disagree.
        other => unreachable!("codegen: no structural order for repr `{other:?}`"),
    }
}

/// Lexicographic chaining: if this position is equal, the answer is `rest`; otherwise it
/// is this position's compare.
///
/// Chaining on *equality* rather than on `cmp(..) != 0` is what keeps the output linear.
/// The obvious form, `(c = cmp(x)) != 0 ? c : rest`, cannot bind `c` inside a C
/// expression, so it has to repeat `cmp(x)` — and a record nested `d` deep would then
/// emit `2^d` copies of its innermost compare.
fn lex_then(types: &TypeTable, r: &Repr, a: &str, b: &str, rest: &str) -> String {
    format!("({} ? {rest} : {})", eq_expr(types, r, a, b), cmp_expr(types, r, a, b))
}

/// Whether an expression of nullable repr `r` holds null, as a C condition. Mirrors
/// `is_null`, which answers the same question for a `Value` rather than an expression.
fn null_test(r: &Repr, e: &str) -> String {
    match r {
        Repr::Nullable(inner) if matches!(inner.as_ref(), Repr::Str) => format!("({e}.data == NULL)"),
        // A closure is a `{fn, env}` pair: a capture-less closure has a NULL env and is
        // not null, so nullability rides on the function pointer.
        Repr::Nullable(inner) if matches!(inner.as_ref(), Repr::Closure { .. }) => {
            format!("({e}.fn == NULL)")
        }
        _ => format!("({e} == NULL)"),
    }
}

/// Content equality, matching `hash_expr`: equal keys must hash equal.
fn eq_expr(types: &TypeTable, r: &Repr, a: &str, b: &str) -> String {
    match r {
        Repr::Str => format!("neon_str_eq({a}, {b})"),
        Repr::Record { fields, .. } => fields
            .iter()
            .map(|(n, fr)| {
                let f = field_name(n);
                eq_expr(types, fr, &format!("{a}.{f}"), &format!("{b}.{f}"))
            })
            .reduce(|x, y| format!("({x} && {y})"))
            .unwrap_or_else(|| "true".into()),
        Repr::Tuple(elems) => elems
            .iter()
            .enumerate()
            .map(|(i, er)| eq_expr(types, er, &format!("{a}._{i}"), &format!("{b}._{i}")))
            .reduce(|x, y| format!("({x} && {y})"))
            .unwrap_or_else(|| "true".into()),
        Repr::F64 | Repr::I64 | Repr::Bool | Repr::Tag => format!("({a} == {b})"),
        // Elementwise, not by address: `[1,2,3] == [1,2,3]` is true, and a list used as a
        // map key finds its entry.
        Repr::List(_) => format!("neon_list_eq({a}, {b})"),
        // Same keys with equal values, regardless of slot order.
        Repr::Map(_, _) => format!("neon_map_eq({a}, {b})"),
        // A handle has no content to compare; two of them are the same file only when they
        // are the same handle. `is_equatable` rejects it, so this is unreachable in a
        // checked program and exists to keep the match honest.
        // Identity, not contents: two handles are the same handle or they are not.
        // The per-type entry that a uniform `Runtime` repr cannot derive.
        Repr::Runtime { .. } => format!("({a} == {b})"),
        // A self-referencing record is a pointer, and comparing it means walking through
        // that pointer -- which a nested expression cannot do, since the walk recurses.
        // `emit_boxed_eq` generates one function per boxed record for exactly this.
        Repr::BoxedRec(_) => match types.boxed_shape(r) {
            Some((name, _)) => format!("{name}_eq({a}, {b})"),
            None => "false".to_string(),
        },
        // One inhabitant: two of them are equal without reading anything. Reading would in
        // fact be wrong -- a `neon_unit` in a union payload is never written, so `memcmp`
        // would decide on uninitialised bytes.
        Repr::Unit | Repr::Null => "true".to_string(),
        // `T | null` for a pointer-backed `T` carries no tag -- null *is* the null pointer
        // -- so the null-ness of each side is tested first, and the payload compared only
        // when both are present. Without this the two pointers were compared directly and
        // two equal-but-distinct lists came back unequal.
        Repr::Nullable(inner) => {
            let (na, nb) = (null_test(r, a), null_test(r, b));
            format!("({na} ? {nb} : (!{nb} && {}))", eq_expr(types, inner, a, b))
        }
        // Same tag, then the payload that tag selects. The `memcmp` fallback below is
        // *wrong* here and was reachable as soon as records compared fieldwise: a union's
        // payload is a C `union`, so the bytes past the live variant are never written, and
        // `Q { tag: :nil } == Q { tag: :nil }` came back false off uninitialised padding.
        Repr::Union(variants) => {
            let arms = variants.iter().enumerate().rev().fold("true".to_string(), |rest, (i, v)| {
                let (pa, pb) = (format!("{a}.u._{i}"), format!("{b}.u._{i}"));
                format!("({a}.tag == {i} ? {} : {rest})", eq_expr(types, v, &pa, &pb))
            });
            format!("({a}.tag == {b}.tag && {arms})")
        }
        _ => format!("(memcmp(&{a}, &{b}, sizeof {a}) == 0)"),
    }
}

/// Structural equality for each boxed (self-referencing) record.
///
/// A generated function rather than an expression, because the walk recurses through the
/// pointer and a C expression cannot. Forward-declared first so that mutually recursive
/// records -- `A` holding a `B` holding an `A` -- can call each other.
///
/// The recursion terminates without a visited set: records are immutable (field and index
/// assignment are *parse* errors), so a value cannot be made to point at itself and the
/// graph is always a DAG. Shared substructure is compared once per path rather than once
/// per node, which is the price of not carrying a visited set.
fn emit_boxed_eq(out: &mut String, types: &TypeTable) {
    let boxed = types.boxed_records();
    for (name, _) in &boxed {
        let _ = writeln!(out, "static bool {name}_eq(const {name}* a, const {name}* b);");
    }
    for (name, shape) in &boxed {
        let _ = writeln!(out, "static bool {name}_eq(const {name}* a, const {name}* b) {{");
        // The same pointer is the same value; one null and one not cannot be equal. A
        // boxed field that is `T | null` arrives here as a null pointer.
        let _ = writeln!(out, "if (a == b) return true;");
        let _ = writeln!(out, "if (a == NULL || b == NULL) return false;");
        // The record sits behind the header in a `value` member, not inline.
        let _ = writeln!(out, "return {};", eq_expr(types, shape, "(a->value)", "(b->value)"));
        let _ = writeln!(out, "}}");
    }
    if !boxed.is_empty() {
        out.push('\n');
    }
}

/// Emit a value-witness (size plus in-place retain/release) for each container element type.
fn emit_witnesses(out: &mut String, types: &TypeTable) {
    for (name, repr) in types.witnesses() {
        let ty = types.c_type(repr);
        let retain = emit_witness_fn(out, types, name, repr, "retain", "neon_retain");
        let release = emit_witness_fn(out, types, name, repr, "release", "neon_release");
        // Structural comparison, so `==` and `<` on a list can walk its elements. `eq`
        // always exists; `cmp` only for an element that has an order.
        let cast = format!("const {ty}* a = (const {ty}*)pa; const {ty}* b = (const {ty}*)pb;");
        let _ = writeln!(
            out,
            "static bool {name}_eq(const void* pa, const void* pb) {{ {cast} return {}; }}",
            eq_expr(types, repr, "(*a)", "(*b)"),
        );
        let cmp = if has_order(repr) {
            let _ = writeln!(
                out,
                "static int {name}_cmp(const void* pa, const void* pb) {{ {cast} return {}; }}",
                cmp_expr(types, repr, "(*a)", "(*b)"),
            );
            format!("{name}_cmp")
        } else {
            "0".to_string()
        };
        let _ = writeln!(
            out,
            "static const neon_witness {name} = {{ sizeof({ty}), {retain}, {release}, {name}_eq, {cmp} }};"
        );
    }
    if !types.witnesses().is_empty() {
        out.push('\n');
    }
}

/// Emit one witness function (retain or release) if the element has counted parts, and
/// return its name; otherwise emit nothing and return `0` (a null function pointer).
fn emit_witness_fn(out: &mut String, types: &TypeTable, name: &str, repr: &Repr, which: &str, func: &str) -> String {
    let mut parts = Vec::new();
    rc_parts(types, func, repr, "(*e)", &mut parts);
    if parts.is_empty() {
        return "0".into();
    }
    let ty = types.c_type(repr);
    let fname = format!("{name}_{which}");
    let _ = writeln!(out, "static void {fname}(void* p) {{ {ty}* e = ({ty}*)p; {}; }}", parts.join("; "));
    fname
}

fn emit_term(out: &mut String, types: &TypeTable, f: &Func, term: &Term) {
    match term {
        // A throwing function returns a tagged result: variant 0 is the value, 1 the error.
        Term::Ret(v) if f.throws.is_some() => {
            let res = f.result_repr().expect("throws implies a result");
            let ty = types.c_type(&res);
            match v {
                Some(v) if !matches!(f.value_repr(*v), Repr::Unit) => {
                    let ok = coerce(types, f, *v, &f.ret);
                    let _ = writeln!(out, "return ({ty}){{ .tag = 0, .u._0 = {ok} }};");
                }
                _ => {
                    let _ = writeln!(out, "return ({ty}){{ .tag = 0 }};");
                }
            }
        }
        Term::Throw(v) if f.throws.is_some() => {
            let res = f.result_repr().expect("throws implies a result");
            let ty = types.c_type(&res);
            let err = coerce(types, f, *v, f.throws.as_ref().unwrap());
            let _ = writeln!(out, "return ({ty}){{ .tag = 1, .u._1 = {err} }};");
        }
        Term::Ret(Some(v)) if matches!(f.value_repr(*v), Repr::Unit) => {
            out.push_str("return;\n");
        }
        Term::Ret(Some(v)) => {
            let _ = writeln!(out, "return {};", coerce(types, f, *v, &f.ret));
        }
        Term::Ret(None) => out.push_str("return;\n"),
        // A throw in a function that declares no `throws` is an error escaping `main`.
        Term::Throw(v) => {
            let _ = writeln!(out, "neon_panic({});", var(*v));
        }
        Term::Jump(t) => emit_jump(out, types, f, t),
        Term::Branch { cond, then, els } => {
            let _ = writeln!(out, "if ({}) {{", var(*cond));
            emit_jump(out, types, f, then);
            out.push_str("} else {\n");
            emit_jump(out, types, f, els);
            out.push_str("}\n");
        }
        Term::Switch { on, arms, default } => {
            let _ = writeln!(out, "switch ({}) {{", var(*on));
            for (k, t) in arms {
                let _ = writeln!(out, "case {}: {{", switch_key(k));
                emit_jump(out, types, f, t);
                out.push_str("}\n");
            }
            out.push_str("default: {\n");
            emit_jump(out, types, f, default);
            out.push_str("}\n    }\n");
        }
        Term::Unreachable => out.push_str("neon_unreachable();\n"),
    }
}

/// A jump: assign the target block's parameters from the arguments (coercing each into the
/// parameter's repr), then `goto`.
fn emit_jump(out: &mut String, types: &TypeTable, f: &Func, t: &Target) {
    let params = &f.block(t.to).params;
    for (&p, &a) in params.iter().zip(&t.args) {
        if p != a {
            let _ = writeln!(out, "{} = {};", var(p), coerce(types, f, a, f.value_repr(p)));
        }
    }
    let _ = writeln!(out, "goto block{};", t.to.0);
}

/// The right-hand side C expression (or statement) for an op. `result` is the value the op
/// defines, when there is one — its repr tells an aggregate constructor which struct to build.
fn op_rhs(types: &TypeTable, f: &Func, result: Option<Value>, op: &Op) -> String {
    match op {
        Op::ConstI64(n) => c_i64(*n),
        Op::ConstF64(bits) => format!("neon_f64_bits({bits}ULL)"),
        Op::ConstBool(b) => b.to_string(),
        Op::ConstStr(s) => format!("neon_str_lit({}, {})", c_string(s), s.len()),
        Op::ConstNull => "neon_null()".into(),
        Op::ConstUnit => "neon_unit_v()".into(),
        Op::ConstAtom(a) => format!("neon_atom({})", atom_hash(a)),
        Op::Prim(p, args) => prim(types, f, *p, args),
        Op::Call { func, args } => {
            let params = types.param_reprs(func);
            let coerced: Vec<String> = args
                .iter()
                .enumerate()
                .map(|(i, &a)| match params.and_then(|p| p.get(i)) {
                    Some(t) => coerce(types, f, a, t),
                    None => var(a),
                })
                .collect();
            format!("{}({})", mangle(func), coerced.join(", "))
        }
        Op::Native { symbol, args } => {
            // A native acts on the narrowed scalar of a nullable, not the tagged wrapper.
            let a: Vec<String> = args.iter().map(|&v| prim_operand(f, v)).collect();
            format!("{symbol}({})", a.join(", "))
        }
        Op::CallClosure { callee, args } => {
            let ret = result.map(|v| f.value_repr(v)).cloned().unwrap_or(Repr::Unit);
            let params: Vec<String> =
                std::iter::once("neon_header*".to_string())
                    .chain(args.iter().map(|&v| types.c_type(f.value_repr(v))))
                    .collect();
            // Call through the stored function pointer, cast to the concrete signature.
            let fnty = format!("{} (*)({})", c_ret_type(types, &ret), params.join(", "));
            let mut a = vec![format!("{}.env", var(*callee))];
            a.extend(args.iter().map(|&v| var(v)));
            format!("(({fnty}){}.fn)({})", var(*callee), a.join(", "))
        }
        Op::MakeRecord { fields, .. } => {
            let repr = result.map(|v| f.value_repr(v)).cloned().unwrap_or(Repr::Unit);
            // A recursive record lives on the heap, so what is built is its pointee shape;
            // the fields are read off that, not off the pointer.
            let shape = types.boxed_shape(&repr).map(|(_, s)| s.clone());
            let ty = match &shape {
                Some(s) => types.c_type(s),
                None => types.c_type(&repr),
            };
            let laid_out = shape.clone().unwrap_or_else(|| repr.clone());
            // Each field value is coerced into the field's declared repr (so a concrete
            // value flowing into a union or nullable field is injected).
            let field_repr = |n: &str| match &laid_out {
                Repr::Record { fields, .. } => fields.iter().find(|(fname, _)| fname == n).map(|(_, r)| r.clone()),
                _ => None,
            };
            let inits: Vec<String> = fields
                .iter()
                .map(|(n, v)| {
                    let val = match field_repr(n) {
                        Some(t) => coerce(types, f, *v, &t),
                        None => var(*v),
                    };
                    format!(".{} = {}", field_name(n), val)
                })
                .collect();
            format!("({ty}){{{}}}", inits.join(", "))
        }
        Op::Field { base, field } => {
            let brepr = f.value_repr(*base);
            // A heap-allocated recursive record is reached through its wrapper.
            if let Some((_, shape)) = types.boxed_shape(brepr) {
                let inner = format!("{}->value", var(*base));
                return match shape {
                    Repr::Union(variants) => {
                        let i =
                            variants.iter().position(|v| record_has_field(v, field)).unwrap_or(0);
                        format!("{inner}.u._{i}.{}", field_name(field))
                    }
                    _ => format!("{inner}.{}", field_name(field)),
                };
            }
            match brepr {
                // Accessing a field of a union value: project to the variant that has it.
                Repr::Union(variants) => {
                    let i = variants.iter().position(|v| record_has_field(v, field)).unwrap_or(0);
                    format!("{}.u._{i}.{}", var(*base), field_name(field))
                }
                _ => format!("{}.{}", var(*base), field_name(field)),
            }
        }
        Op::MakeTuple(elems) => {
            let repr = result.map(|v| f.value_repr(v)).cloned().unwrap_or(Repr::Unit);
            let ty = types.c_type(&repr);
            let inits: Vec<String> =
                elems.iter().enumerate().map(|(i, v)| format!("._{i} = {}", var(*v))).collect();
            format!("({ty}){{{}}}", inits.join(", "))
        }
        Op::Elem { base, index } => format!("{}._{index}", var(*base)),
        Op::Retain(v) => rc_value(types, f, "neon_retain", *v),
        Op::Release(v) => rc_value(types, f, "neon_release", *v),
        Op::IsErr(v) => format!("({}.tag == 1)", var(*v)),
        Op::UnwrapOk(v) => format!("{}.u._0", var(*v)),
        Op::UnwrapErr(v) => format!("{}.u._1", var(*v)),
        Op::IsNull(v) => is_null(f, *v),
        Op::IsVariant { value, variant } => match f.value_repr(*value) {
            Repr::Union(variants) => {
                let i = variants
                    .iter()
                    .position(|v| variant_name(v).as_deref() == Some(variant.as_str()))
                    .unwrap_or(0);
                format!("({}.tag == {i})", var(*value))
            }
            // An erased value carries its concrete type as a tag in its box.
            Repr::Any => format!("(neon_box_tag({}) == {}ULL)", var(*value), fnv1a(variant)),
            // A value of one concrete type is that variant only if it *is* that type —
            // `r is Green` where `r` is a `Red` is false, not vacuously true.
            other => (variant_name(other).as_deref() == Some(variant.as_str())).to_string(),
        },
        Op::Cast(v) => {
            let target = result.map(|r| f.value_repr(r).clone()).unwrap_or(Repr::Any);
            cast_expr(types, &var(*v), f.value_repr(*v), &target)
        }
        // Every op reachable in a lowered program has a case above. Emitting a plausible
        // `0` for anything else produced a program that ran and answered wrongly, with the
        // only evidence a comment in generated C nobody reads.
        other => unreachable!("codegen: no emission for op `{}`", op_name(other)),
    }
}

/// A cast (`as`, and the projection an `orelse`/narrowing produces): move a value between a
/// union and one of its variants. Widening injects (`{tag, payload}`); narrowing projects
/// the payload; a nullable pointer casts to/from its pointer identically.
fn cast_expr(types: &TypeTable, expr: &str, src: &Repr, target: &Repr) -> String {
    if src == target {
        return expr.to_string();
    }
    // A newtype and its representation: unwrap or wrap the single hidden payload.
    if let Some(inner) = newtype_inner(src) {
        if inner == target {
            return format!("{expr}.{}", field_name("#inner"));
        }
    }
    if let Some(inner) = newtype_inner(target) {
        if inner == src {
            return format!("({}){{ .{} = {expr} }}", types.c_type(target), field_name("#inner"));
        }
    }
    // Recovering a concrete value from `any`: read the payload back out of the box.
    if matches!(src, Repr::Any) && !matches!(target, Repr::Any) {
        return format!("(*({}*)neon_box_payload({expr}))", types.c_type(target));
    }
    // Narrowing a union to one of its variants reads that payload; every other direction
    // (injecting a variant, null into a nullable pointer, an identity pointer cast) is the
    // same coercion a flow site would apply.
    if let Repr::Union(variants) = src {
        if !matches!(target, Repr::Union(_)) {
            if let Some(i) = variants.iter().position(|vr| vr == target) {
                return format!("{expr}.u._{i}");
            }
        }
    }
    coerce_expr(types, expr, src, target)
}

/// Test whether a value is `null`. A union carries a tag (the null variant's index); a
/// nullable pointer is null when the pointer (or a string's data) is NULL.
fn is_null(f: &Func, v: Value) -> String {
    match f.value_repr(v) {
        Repr::Union(variants) => {
            let i = variants.iter().position(|r| matches!(r, Repr::Null)).unwrap_or(0);
            format!("({}.tag == {i})", var(v))
        }
        Repr::Nullable(inner) if matches!(inner.as_ref(), Repr::Str) => {
            format!("({}.data == NULL)", var(v))
        }
        // A closure is a `{fn, env}` pair, so nullability rides on the function pointer:
        // a capture-less closure legitimately has a NULL environment and is not null.
        Repr::Nullable(inner) if matches!(inner.as_ref(), Repr::Closure { .. }) => {
            format!("({}.fn == NULL)", var(v))
        }
        Repr::Nullable(_) => format!("({} == NULL)", var(v)),
        Repr::Null => "true".into(),
        // An erased value is null when its box carries the null type tag.
        Repr::Any => format!("(neon_box_tag({}) == {}ULL)", var(v), fnv1a("null")),
        _ => "false".into(),
    }
}

/// A newtype's payload repr — the sole `#inner` field of a nominal wrapper. A newtype is
/// nominally distinct but holds exactly one value, so `as` moves that value in or out.
fn newtype_inner(r: &Repr) -> Option<&Repr> {
    match r {
        Repr::Record { fields, .. } if fields.len() == 1 && fields[0].0 == "#inner" => {
            Some(&fields[0].1)
        }
        _ => None,
    }
}

/// The scalar a union collapses to once its `null` variant is excluded — the type an
/// arithmetic or comparison on a narrowed nullable operates on.
fn scalar_repr(r: &Repr) -> &Repr {
    match r {
        Repr::Union(variants) => variants.iter().find(|v| !matches!(v, Repr::Null)).unwrap_or(r),
        _ => r,
    }
}

/// Equality between a union and a scalar variant: matches only when the tag selects that
/// variant and the payloads are equal. `None` when neither operand is a union.
fn union_compare(types: &TypeTable, f: &Func, op: PrimOp, args: &[Value]) -> Option<String> {
    let (a, b) = (*args.first()?, *args.get(1)?);
    let (variants, u, other_repr, other) = match (f.value_repr(a), f.value_repr(b)) {
        (Repr::Union(vs), other) => (vs, a, other, b),
        (other, Repr::Union(vs)) => (vs, b, other, a),
        _ => return None,
    };
    let i = variants.iter().position(|r| r == other_repr)?;
    // The payload compares *structurally*, like anything else: a raw C `==` here worked
    // only while the variant was a scalar, and emitted `nr0 == nr0` -- which C rejects
    // outright -- the moment the variant was a record, a tuple or a `str`.
    let eq = format!(
        "({}.tag == {i} && {})",
        var(u),
        eq_expr(types, other_repr, &format!("{}.u._{i}", var(u)), &prim_operand(f, other)),
    );
    Some(match op {
        PrimOp::Ne => format!("(!{eq})"),
        _ => eq,
    })
}

/// A primitive's operand: a union value is projected to its scalar variant first, since the
/// operation acts on the narrowed type, not the tagged wrapper.
fn prim_operand(f: &Func, v: Value) -> String {
    match f.value_repr(v) {
        Repr::Union(variants) => {
            let i = variants.iter().position(|r| !matches!(r, Repr::Null)).unwrap_or(0);
            format!("{}.u._{i}", var(v))
        }
        _ => var(v),
    }
}

/// The name an `is Name` test asks for. A union's variants are not only records: a union
/// of primitives (`i64 | str | bool`) is tested by type name, so this uses the same naming
/// as the boxed type tag rather than recognising records alone.
fn variant_name(r: &Repr) -> Option<String> {
    match r {
        Repr::Record { name, .. } => name.clone(),
        Repr::Var(_) | Repr::Never | Repr::Any => None,
        other => Some(crate::backend::ctype::type_tag_name(other)),
    }
}

/// Whether a variant is a record carrying the named field.
fn record_has_field(r: &Repr, field: &str) -> bool {
    matches!(r, Repr::Record { fields, .. } if fields.iter().any(|(n, _)| n == field))
}

/// Coerce a value into a target repr at a flow site. A concrete value flowing into a union
/// is injected as `{tag, payload}`; `null` into a nullable pointer becomes NULL; a value
/// whose repr already matches passes through.
/// The address of a value as the container's slot type. A container copies
/// `witness->size` bytes through this pointer, so a value that has not been injected into
/// the slot's type first is read past its own end — a `1.0` handed to a `Map[str, Json]`
/// is eight bytes where the witness promises the whole union. The one-element array
/// literal gives the coerced value an address without a named temporary, and decays to the
/// pointer the native wants.
fn addr_of(types: &TypeTable, f: &Func, v: Value, target: &Repr) -> String {
    if f.value_repr(v) == types.resolve(target) {
        return format!("&{}", var(v));
    }
    format!("({}[]){{{}}}", types.c_type(target), coerce(types, f, v, target))
}

fn coerce(types: &TypeTable, f: &Func, v: Value, target: &Repr) -> String {
    coerce_expr(types, &var(v), f.value_repr(v), target)
}

fn coerce_expr(types: &TypeTable, expr: &str, src: &Repr, target: &Repr) -> String {
    // Resolve back-edges first, or none of the shape tests below fire: injecting an atom
    // into `mu type A = :ok | List[A]` saw a `Recursive` rather than the union it names,
    // matched nothing, and fell through to a zeroed literal that silently dropped the
    // value. Resolving also makes a back-edge and its unfolding compare equal, which is
    // what lets the identity pass through untouched.
    let src = types.resolve(src);
    let target = types.resolve(target);
    if src == target || matches!(src, Repr::Never) {
        return expr.to_string();
    }
    // Erasing into `any`: box the value with its witness and type tag. The one-element
    // array compound literal gives the payload an address without needing a named temp.
    if matches!(target, Repr::Any) {
        return format!(
            "neon_box_new(({}[]){{{expr}}}, {}, {}ULL)",
            types.c_type(src),
            types.witness_ref(src),
            type_tag(src),
        );
    }
    if let Repr::Union(variants) = target {
        if let Some(i) = variants.iter().position(|vr| vr == src) {
            return format!("({}){{ .tag = {i}, .u._{i} = {expr} }}", types.c_type(target));
        }
    }
    // Covariant/width subtyping: rebuild the target aggregate, coercing each field from the
    // source (a `Box[i64]` becomes a `Box[i64|str]`, a `User` a `{name}`).
    if let (Repr::Record { fields: sf, .. }, Repr::Record { fields: tf, .. }) = (src, target) {
        let inits: Vec<String> = tf
            .iter()
            .map(|(n, tr)| {
                let init = match sf.iter().find(|(sn, _)| sn == n) {
                    Some((_, sr)) => {
                        coerce_expr(types, &format!("{expr}.{}", field_name(n)), sr, tr)
                    }
                    // A field the source lacks is an optional one, and an absent optional
                    // is *null* — not zero. Zeroing a `i64 | null` picks tag 0, which is
                    // the `i64` variant holding 0, so the field reads as present.
                    None => coerce_expr(types, "neon_null()", &Repr::Null, tr),
                };
                format!(".{} = {}", field_name(n), init)
            })
            .collect();
        return format!("({}){{{}}}", types.c_type(target), inits.join(", "));
    }
    if let (Repr::Tuple(se), Repr::Tuple(te)) = (src, target) {
        let inits: Vec<String> = te
            .iter()
            .enumerate()
            .map(|(i, tr)| {
                let sr = se.get(i).unwrap_or(tr);
                format!("._{i} = {}", coerce_expr(types, &format!("{expr}._{i}"), sr, tr))
            })
            .collect();
        return format!("({}){{{}}}", types.c_type(target), inits.join(", "));
    }
    // Reprs that share a C representation (a nullable pointer and its pointer, a nullable
    // string and a string) need no conversion.
    if types.c_type(src) == types.c_type(target) {
        return expr.to_string();
    }
    // What is left cannot happen at run time: it is the arm of a test the checker already
    // decided — casting a statically-`null` value to a string, or the `f64` arm of a union
    // the value is known not to hold. A zero of the target (a C99 compound literal, valid
    // for structs, scalars and pointers alike) keeps that dead branch compiling.
    format!("({}){{0}}", types.c_type(target))
}

/// A retain or release, recursing into an inline aggregate so every counted field is
/// touched. Yields a comma expression (or `(void)0` when nothing is counted).
fn rc_value(types: &TypeTable, f: &Func, func: &str, v: Value) -> String {
    let mut parts = Vec::new();
    rc_parts(types, func, f.value_repr(v), &var(v), &mut parts);
    if parts.is_empty() {
        "(void)0".into()
    } else {
        parts.join(", ")
    }
}

/// Append the retain/release calls for the counted parts of a value of `repr` at C
/// expression `expr`.
fn rc_parts(types: &TypeTable, func: &str, repr: &Repr, expr: &str, out: &mut Vec<String>) {
    rc_parts_rec(types, func, repr, expr, out, &mut Vec::new())
}

/// `seen` is the chain of back-edges currently being resolved. Re-entering one means the
/// cycle closes entirely by value — `record Node { next: Node | null }` — with no pointer
/// anywhere to terminate it. Such a type has no finite layout and cannot be counted in
/// place; it needs heap-allocating, which is not implemented yet. Stopping here keeps the
/// emitter from recursing until the stack dies, and `cc` then rejects the infinite struct
/// with a diagnostic naming the type.
fn rc_parts_rec(
    types: &TypeTable,
    func: &str,
    repr: &Repr,
    expr: &str,
    out: &mut Vec<String>,
    seen: &mut Vec<crate::typecheck::types::TyId>,
) {
    if types.is_boxed(repr) {
        out.push(format!("{func}((neon_header*){expr})"));
        return;
    }
    if let Repr::Recursive(ty) = repr {
        if seen.contains(ty) {
            return;
        }
        seen.push(*ty);
        let resolved = types.resolve(repr).clone();
        rc_parts_rec(types, func, &resolved, expr, out, seen);
        seen.pop();
        return;
    }
    match repr {
        Repr::Str => out.push(format!("{func}({expr}.owner)")),
        Repr::Closure { .. } => out.push(format!("{func}({expr}.env)")),
        Repr::List(_) | Repr::Map(_, _) | Repr::Runtime { .. } | Repr::Any => {
            out.push(format!("{func}((neon_header*){expr})"))
        }
        // A union is an inline `{tag, payload}`, so only the live variant's counted parts
        // are touched — selected by the tag at run time.
        Repr::Union(variants) => {
            let mut chain = String::new();
            for (i, v) in variants.iter().enumerate() {
                let mut sub = Vec::new();
                rc_parts_rec(types, func, v, &format!("{expr}.u._{i}"), &mut sub, seen);
                if !sub.is_empty() {
                    let _ = write!(
                        chain,
                        "{expr}.tag == {i} ? ((void)({})) : ",
                        sub.join(", ")
                    );
                }
            }
            if !chain.is_empty() {
                out.push(format!("({chain}((void)0))"));
            }
        }
        // A nullable pointer has the same layout as the pointer — `null` *is* the null
        // pointer — so it is counted the same way, and `neon_retain`/`release` already
        // no-op on NULL. Assuming the payload was always a bare pointer emitted a header
        // cast against a `neon_closure`, which is a `{fn, env}` struct.
        Repr::Nullable(inner) => rc_parts_rec(types, func, inner, expr, out, seen),
        Repr::Record { fields, .. } => {
            for (n, fr) in fields {
                rc_parts_rec(types, func, fr, &format!("{expr}.{}", field_name(n)), out, seen);
            }
        }
        Repr::Tuple(elems) => {
            for (i, e) in elems.iter().enumerate() {
                rc_parts_rec(types, func, e, &format!("{expr}._{i}"), out, seen);
            }
        }
        _ => {}
    }
}

/// The C relational operator for an ordering primop, for comparing a three-way result
/// against zero.
fn rel_op(op: PrimOp) -> &'static str {
    match op {
        PrimOp::Lt => "<",
        PrimOp::Le => "<=",
        PrimOp::Gt => ">",
        PrimOp::Ge => ">=",
        _ => unreachable!("rel_op on a non-ordering primop"),
    }
}

fn prim(types: &TypeTable, f: &Func, op: PrimOp, args: &[Value]) -> String {
    // Comparing a union against one of its variants must check the tag: a `null` value has
    // a zero-initialised payload, so a bare `payload == 0` would wrongly match `0`.
    if matches!(op, PrimOp::Eq | PrimOp::Ne) {
        if let Some(s) = union_compare(types, f, op, args) {
            return s;
        }
        // A nullable against a literal `null`: the two sides have different reprs -- a
        // value initialised with `null` is lowered as `Repr::Null` (a `neon_unit`), not as
        // the nullable it was annotated with -- so this is a null *test*, not a compare.
        if let (Some(&x), Some(&y)) = (args.first(), args.get(1)) {
            let (rx, ry) = (f.value_repr(x), f.value_repr(y));
            let test = match (rx, ry) {
                (Repr::Nullable(_), Repr::Null) => Some(null_test(rx, &var(x))),
                (Repr::Null, Repr::Nullable(_)) => Some(null_test(ry, &var(y))),
                _ => None,
            };
            if let Some(t) = test {
                return match op {
                    PrimOp::Ne => format!("(!{t})"),
                    _ => t,
                };
            }
        }
        // Two unions: `union_compare` handles only union-against-a-bare-variant, and
        // `prim_operand` below would project *both* sides to their first variant and
        // compare those -- so `(i64 | bool)` operands compared an i64 against a bool and
        // `1 == true` was true. Compare the tagged values whole instead.
        if let (Some(&x), Some(&y)) = (args.first(), args.get(1)) {
            let (rx, ry) = (f.value_repr(x), f.value_repr(y));
            if matches!(rx, Repr::Union(_)) && rx == ry {
                let eq = eq_expr(types, rx, &var(x), &var(y));
                return match op {
                    PrimOp::Ne => format!("(!{eq})"),
                    _ => eq,
                };
            }
        }
    }
    let scalar = |v: Value| scalar_repr(f.value_repr(v));
    let is_float = args.first().is_some_and(|&v| matches!(scalar(v), Repr::F64));
    let is_str = args.first().is_some_and(|&v| matches!(scalar(v), Repr::Str));
    let a = args.first().map(|&v| prim_operand(f, v)).unwrap_or_default();
    let b = args.get(1).map(|&v| prim_operand(f, v)).unwrap_or_default();

    // Comparison is structural on every type (docs/decisions.md). C has no `==` for a
    // struct, so an aggregate expands fieldwise, and `str` ordering needs the runtime's
    // bytewise compare. Scalars fall through to the plain operators below.
    if matches!(op, PrimOp::Eq | PrimOp::Ne | PrimOp::Lt | PrimOp::Le | PrimOp::Gt | PrimOp::Ge) {
        let r = args.first().map(|&v| scalar(v));
        // `Null`/`Unit` are `neon_unit` structs with one inhabitant: C cannot `==` them,
        // and `eq_expr` answers `true` without reading bytes that were never written.
        let aggregate = matches!(
            r,
            Some(
                Repr::Record { .. }
                    | Repr::Tuple(_)
                    | Repr::List(_)
                    | Repr::Map(_, _)
                    | Repr::BoxedRec(_)
                    | Repr::Nullable(_)
                    | Repr::Null
                    | Repr::Unit
            )
        );
        if let Some(r) = r {
            if aggregate {
                return match op {
                    PrimOp::Eq => eq_expr(types, r, &a, &b),
                    PrimOp::Ne => format!("(!{})", eq_expr(types, r, &a, &b)),
                    _ => format!("({} {} 0)", cmp_expr(types, r, &a, &b), rel_op(op)),
                };
            }
            if matches!(r, Repr::Str) && !matches!(op, PrimOp::Eq | PrimOp::Ne) {
                return format!("(neon_str_cmp({a}, {b}) {} 0)", rel_op(op));
            }
        }
    }
    match op {
        // i64 arithmetic traps on overflow; f64 is plain; str `+` is a borrowing concat.
        PrimOp::Add if is_str => format!("neon_str_add({a}, {b})"),
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
        // The shift amount is masked to the operand width, so any i64 amount is defined.
        PrimOp::Bsl => format!("((int64_t)((uint64_t){a} << ({b} & 63)))"),
        PrimOp::Bsr => format!("({a} >> ({b} & 63))"),
        PrimOp::Bnot => format!("(~{a})"),
    }
}

// ---- C names ----

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
