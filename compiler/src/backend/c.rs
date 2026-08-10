//! The C backend: an IR `Program` becomes one C translation unit, compiled and linked
//! against `runtime/` by `cc`. See `docs/design/ir.md`.
//!
//! Reprs become C types (`i64`→`int64_t`, `str`→`neon_str`, a record→a named `struct`, …);
//! SSA values become function-scoped locals; a block becomes a label; a terminator becomes
//! a `goto`, `return`, or `switch`, with block arguments assigned at the edge before the
//! jump — which is how SSA-with-block-arguments lowers to C without φ-nodes.
//!
//! Three conventions run through the whole file and explain most of what looks odd:
//!
//! - a **throwing** function does not return its declared type. It returns the tagged
//!   union `{ok, err}` that `Func::result_repr` builds, and `ret`/`throw` become an
//!   injection into tag 0 or tag 1. Anything that computes a C return type has to go
//!   through `fn_ret_type` for that reason, adapter thunks included.
//! - a value crossing into a *wider* slot is **coerced**, never bit-copied: injected into
//!   a union, boxed into `any`, or rebuilt field-by-field for width subtyping. Every flow
//!   site — call argument, block argument, list element, record field, `return` — runs
//!   `coerce`, because a value stored at its own narrow width in a wide slot is read
//!   back past its end.
//! - the runtime is generic over element types through **witnesses**: a size plus
//!   retain/release/eq/cmp function pointers, emitted here per element repr, since the
//!   runtime cannot know the layouts codegen invents. Natives that take an element
//!   therefore take it by address rather than by value.

use crate::backend::ctype::{field_name, fnv1a, ice, TypeTable};
use crate::ir::repr::Repr;
use crate::ir::ssa::{Block, Func, Op, PrimOp, Program, SwitchKey, Target, Term, Value};
use std::fmt::Write;

/// Emit the whole program as C source.
pub fn emit(program: &Program) -> String {
    emit_with(program, Entry::Main)
}

/// Emit a test binary: the same translation unit, but the entry point dispatches to one
/// `test` block instead of calling `main`.
///
/// **One test per process, selected by `NEON_TEST`.** The alternative — an entry point that
/// walks the whole table — cannot survive the first failure, because a failed assertion
/// calls `neon_panic`, which exits. Running each block in its own process is what lets the
/// runner report the second test after the first one has died, and it contains a crash or a
/// leaked global as well as it contains an assertion.
///
/// The selector is an environment variable rather than `argv` because argv belongs to the
/// program under test: `os::args()` inside a test block must see the real command line,
/// not a harness selector pretending to be one. `getenv` reaches the same information
/// without stealing from that namespace.
pub fn emit_tests(program: &Program, tests: &[crate::ir::lower::TestEntry]) -> String {
    emit_with(program, Entry::Tests(tests))
}

/// As `emit_tests`, for `bench` blocks: the entry point runs ONE bench, chosen by
/// `NEON_BENCH`, through the adaptive timing loop `emit_bench_entry` explains.
pub fn emit_benches(program: &Program, benches: &[crate::ir::lower::TestEntry]) -> String {
    emit_with(program, Entry::Benches(benches))
}

/// Which entry point the emitted C ends with.
enum Entry<'a> {
    /// `int main` calling the program's own `main`, when it has one.
    Main,
    Tests(&'a [crate::ir::lower::TestEntry]),
    Benches(&'a [crate::ir::lower::TestEntry]),
}

fn emit_with(program: &Program, entry: Entry) -> String {
    let types = TypeTable::build(program);
    let mut out = String::new();
    // <math.h> for `fmod`, float `%`'s spelling; `-lm` is already on the link line for
    // the runtime's own math.c.
    out.push_str("#include \"libneon_rt.h\"\n#include <math.h>\n\n");

    // Aggregate struct definitions, before any function that uses them.
    types.emit_defs(&mut out);
    // Before the witnesses: an element witness for a boxed record calls into these.
    emit_boxed_eq(&mut out, &types);
    // Copies are declared before the witness wrappers that call them, defined after the
    // env drops whose symbols they allocate with.
    emit_boxed_copy_decls(&mut out, &types);
    emit_witnesses(&mut out, &types);
    emit_key_witnesses(&mut out, &types);
    emit_env_drops(&mut out, &types);
    emit_boxed_copy_defs(&mut out, &types);
    emit_env_copies(&mut out, &types);

    // Forward declarations, so call order does not matter.
    for f in &program.funcs {
        let _ = writeln!(
            out,
            "{};",
            signature(&types, f, program.inlined.contains(&f.name))
        );
    }
    out.push('\n');

    // Adapter thunks give ordinary functions used as closure values the closure ABI.
    emit_thunks(&mut out, &types, program);
    emit_resource_drops(&mut out, &types, program);
    emit_map_updaters(&mut out, &types, program);
    emit_spawn_shims(&mut out, &types, program);

    // Function bodies go to their own buffer: emitting one is what REQUESTS a list
    // conversion shim, and the shims have to be declared above the bodies that call them.
    let mut body = String::new();
    for f in &program.funcs {
        emit_fn(&mut body, &types, f, program.inlined.contains(&f.name));
        body.push('\n');
    }
    emit_list_convs(&mut out, &types);
    out.push_str(&body);

    match entry {
        Entry::Tests(tests) => emit_test_entry(&mut out, tests),
        Entry::Benches(benches) => emit_bench_entry(&mut out, benches),
        // The C entry point, if this program has a `main`.
        Entry::Main => {
            if program.funcs.iter().any(|f| f.name == "main") {
                out.push_str(
                    "int main(int argc, char** argv) {\n    neon_rt_init(argc, argv);\n    nl_main();\n    return 0;\n}\n",
                );
            }
        }
    }
    reindent(&out)
}

/// The test binary's entry point: run the one block `NEON_TEST` names, and nothing else.
///
/// Exit 0 for a block that returned, 2 for a selector that named no test. A *failed*
/// assertion never reaches either — `neon_panic` exits the process itself — which is
/// exactly the split the runner reads: a clean 0 is a pass, anything else is a failure with
/// the panic's message on stderr.
/// The bench binary's entry point: run the one block `NEON_BENCH` names through an
/// adaptive timing loop, and print nanoseconds-per-iteration to stdout as a float.
///
/// The loop is Go's shape: one untimed call to warm caches (and to fail fast if the
/// body traps), then batches that double until a batch takes at least 200ms — long
/// enough that clock granularity and loop overhead vanish into the noise, short enough
/// that a whole suite stays interactive. The body is reached through a FUNCTION
/// POINTER on purpose: the C compiler cannot see through it to hoist or delete the
/// repeated work, which is the classic way a benchmark measures nothing.
///
/// One process per bench, like tests: a bench that traps kills only itself, and each
/// starts from a cold runtime rather than inheriting the previous one's heap.
fn emit_bench_entry(out: &mut String, benches: &[crate::ir::lower::TestEntry]) {
    out.push_str("#include <stdlib.h>\n#include <stdio.h>\n\n");
    out.push_str("static void neon_bench_run(void (*body)(void)) {\n");
    out.push_str("    body();\n");
    out.push_str("    int64_t k = 1;\n");
    out.push_str("    for (;;) {\n");
    out.push_str("        int64_t began = neon_time_monotonic();\n");
    out.push_str("        for (int64_t i = 0; i < k; i++) {\n            body();\n        }\n");
    out.push_str("        int64_t took = neon_time_monotonic() - began;\n");
    out.push_str("        if (took >= 200000000 || k >= (1LL << 40)) {\n");
    out.push_str("            printf(\"%.1f\\n\", (double)took / (double)k);\n");
    out.push_str("            return;\n        }\n");
    out.push_str("        k += k;\n    }\n}\n\n");
    out.push_str("int main(int argc, char** argv) {\n");
    out.push_str("    const char *which = getenv(\"NEON_BENCH\");\n");
    out.push_str("    if (which == NULL) {\n");
    out.push_str(
        "        fputs(\"neon: this is a bench binary; set NEON_BENCH to a bench index\\n\", stderr);\n",
    );
    out.push_str("        return 2;\n    }\n");
    out.push_str("    neon_rt_init(argc, argv);\n");
    out.push_str("    switch (strtol(which, NULL, 10)) {\n");
    for (i, b) in benches.iter().enumerate() {
        let _ = writeln!(
            out,
            "    case {i}: neon_bench_run({}); return 0;",
            mangle(&b.symbol)
        );
    }
    out.push_str("    default: break;\n    }\n");
    out.push_str("    fputs(\"neon: no such bench\\n\", stderr);\n");
    out.push_str("    return 2;\n}\n");
}

fn emit_test_entry(out: &mut String, tests: &[crate::ir::lower::TestEntry]) {
    // `getenv`/`strtol` and `fputs`; the runtime header does not promise either.
    out.push_str("#include <stdlib.h>\n#include <stdio.h>\n\n");
    out.push_str("int main(int argc, char** argv) {\n");
    out.push_str("    const char *which = getenv(\"NEON_TEST\");\n");
    out.push_str("    if (which == NULL) {\n");
    out.push_str(
        "        fputs(\"neon: this is a test binary; set NEON_TEST to a test index\\n\", stderr);\n",
    );
    out.push_str("        return 2;\n    }\n");
    out.push_str("    neon_rt_init(argc, argv);\n");
    out.push_str("    switch (strtol(which, NULL, 10)) {\n");
    for (i, t) in tests.iter().enumerate() {
        let _ = writeln!(out, "    case {i}: {}(); return 0;", mangle(&t.symbol));
    }
    out.push_str("    default: break;\n    }\n");
    out.push_str("    fputs(\"neon: no such test\\n\", stderr);\n");
    out.push_str("    return 2;\n}\n");
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
fn signature(types: &TypeTable, f: &Func, inlined: bool) -> String {
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
    let params = if params.is_empty() {
        "void".to_string()
    } else {
        params.join(", ")
    };
    // `NEON_INLINE` rather than the GNU attribute spelled out: it carries `static` and the
    // `inline` keyword along with the force, because MSVC's `__forceinline` *is* `inline`
    // and cannot be written beside it. `neon/portability.h` holds the three spellings; see
    // it for why the storage class belongs inside the macro rather than beside it.
    let qual = if inlined { "NEON_INLINE " } else { "" };
    format!(
        "{qual}{} {}({})",
        fn_ret_type(types, f),
        mangle(&f.name),
        params
    )
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

/// One function body. Every SSA value is declared up front as a *function-scoped* local
/// rather than at the instruction that defines it, so that a value defined in one block
/// and read in another is in scope at both — blocks lower to labels and `goto`s, and some
/// (a `switch` arm) sit inside braces that a declaration would not escape. Block
/// parameters need this too: `emit_jump` assigns them on the edge, before the jump, which
/// is only possible if the parameter's storage outlives the block that owns it.
fn emit_fn(out: &mut String, types: &TypeTable, f: &Func, inlined: bool) {
    let _ = writeln!(out, "{} {{", signature(types, f, inlined));

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

/// One block: a label, its instructions, its terminator. The label is followed by an empty
/// statement (`block0:;`) because C requires a label to precede a *statement*, and a block
/// whose first line is a declaration — or which is empty but for its terminator's braces —
/// would otherwise not compile.
fn emit_block(out: &mut String, types: &TypeTable, f: &Func, b: &Block) {
    let _ = writeln!(out, "block{}:; ", b.id.0);
    for inst in &b.insts {
        emit_inst(out, types, f, inst);
    }
    emit_term(out, types, f, b.id.0, &b.term);
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
        Op::MakeRecord { .. } if inst.result.is_some_and(|v| types.is_boxed(f.value_repr(v))) => {
            let r = inst.result.unwrap();
            let (wrapper, shape) = types.boxed_shape(f.value_repr(r)).unwrap();
            let sty = types.c_type(shape);
            let _ = writeln!(
                out,
                "{} = ({wrapper}*)neon_alloc(sizeof({sty}), {});",
                var(r),
                types.env_drop_ref(shape, false)
            );
            let _ = writeln!(
                out,
                "{}->value = {};",
                var(r),
                op_rhs(types, f, inst.result, &inst.op)
            );
        }
        Op::Index {
            base,
            index,
            covered,
        } => emit_index(out, types, f, inst.result, *base, *index, *covered),
        // A receive fills a temporary of the channel's element type and reports whether a
        // value arrived; the Neon result is `T | null`, so the two outcomes are coerced
        // into it here — the runtime cannot build that union, only this emitter knows it.
        // An await fills a temporary of the task's result type; unlike recv there is no
        // null outcome — the call parks until the result exists.
        Op::Native { symbol, args } if symbol == "neon_task_lang_await" => {
            let Some(r) = inst.result else {
                return;
            };
            let e = chan_elem(types, f, args[0]); // Task[T] is Runtime{args:[T]}, like Channel
            let ec = types.c_type(&e);
            let res = f.value_repr(r).clone();
            let got = coerce_expr(types, "_tav", &e, &res);
            let _ = writeln!(
                out,
                "{{ {ec} _tav; neon_task_lang_await({}, &_tav); {} = {got}; }}",
                var(args[0]),
                var(r),
            );
        }
        Op::Native { symbol, args }
            if symbol == "neon_channel_recv" || symbol == "neon_channel_recv_timeout" =>
        {
            let Some(r) = inst.result else {
                return;
            };
            let e = chan_elem(types, f, args[0]);
            let ec = types.c_type(&e);
            let res = f.value_repr(r).clone();
            let got = coerce_expr(types, "_crv", &e, &res);
            let none = coerce_expr(types, "((neon_unit){0})", &Repr::Null, &res);
            let call = if symbol == "neon_channel_recv_timeout" {
                format!(
                    "neon_channel_recv_timeout({}, &_crv, {})",
                    var(args[0]),
                    var(args[1])
                )
            } else {
                format!("neon_channel_recv({}, &_crv)", var(args[0]))
            };
            let _ = writeln!(
                out,
                "{{ {ec} _crv; if ({call}) {{ {} = {got}; }} else {{ {} = {none}; }} }}",
                var(r),
                var(r),
            );
        }
        Op::Native { symbol, args } if is_list_builder(symbol) => {
            emit_list_builder(out, types, f, inst.result, symbol, args)
        }
        // A native whose Neon signature returns a tuple takes the extra slots as C
        // out-parameters. See `emit_native_out`.
        Op::Native { symbol, args }
            if inst
                .result
                .is_some_and(|v| matches!(f.value_repr(v), Repr::Tuple(_))) =>
        {
            emit_native_out(out, types, f, inst.result, symbol, args)
        }
        Op::MakeClosure { func, captures } => {
            emit_make_closure(out, types, f, inst.result, func, captures)
        }
        _ => {
            let rhs = op_rhs(types, f, inst.result, &inst.op);
            match inst.result {
                // A void-typed result is a bare statement: a unit-returning call, or a
                // `never` one — `neon_os_exit` is C `void`, and its "result" is never
                // read because the lowerer terminates the block right after the call.
                Some(v) if !matches!(f.value_repr(v), Repr::Unit | Repr::Never) => {
                    let _ = writeln!(out, "{} = {};", var(v), rhs);
                }
                _ => {
                    let _ = writeln!(out, "{};", rhs);
                }
            }
        }
    }
}

/// A value's list element repr, looking through a union it may be injected into.
///
/// Ices rather than falling back to `any`. A list builder's result *is* a list, so a miss
/// means the repr was never pinned — and `any` was not a safe thing to guess: it has no
/// interned value-witness, so the caller then emitted a null witness and the runtime lost
/// the element size it copies slots by.
fn list_elem(types: &TypeTable, f: &Func, v: Value) -> Repr {
    match list_variant(types, f.value_repr(v)) {
        Some(Repr::List(e)) => *e,
        _ => ice(f.value_repr(v), "a list builder whose result is not a list"),
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
fn emit_make_list(
    out: &mut String,
    types: &TypeTable,
    f: &Func,
    result: Option<Value>,
    elems: &[Value],
) {
    let Some(r) = result else { return };
    let target = types.resolve(f.value_repr(r)).clone();
    // What is built is always a list; the value's own repr may be a union it injects into.
    // Both misses ice rather than defaulting to `List[any]`: the element repr chooses the
    // witness the slots are sized and released by, so an `any` guessed here writes each
    // element at the wrong width and hands the runtime a witness that does not exist.
    let list = match list_variant(types, &target) {
        Some(l) => l,
        None => ice(&target, "a list literal whose repr contains no list"),
    };
    let elem = match &list {
        Repr::List(e) => (**e).clone(),
        other => ice(other, "a list variant that is not a list"),
    };
    let ety = types.c_type(&elem);
    let n = elems.len();
    let direct = target == list;
    let dest = if direct {
        var(r)
    } else {
        format!("{}_l", var(r))
    };
    if !direct {
        let _ = writeln!(out, "neon_list* {dest};");
    }
    let _ = writeln!(
        out,
        "{dest} = neon_list_new_with_capacity({}, {n});",
        types.witness_ref(&elem)
    );
    // Each element is coerced into the list's element repr, so a concrete value flowing
    // into a `List[i64 | null]` or a covariant `List[Shape]` is injected on the way in.
    for (i, &v) in elems.iter().enumerate() {
        let val = coerce(types, f, v, &elem);
        let _ = writeln!(out, "(({ety}*){dest}->data)[{i}] = {val};");
    }
    let _ = writeln!(out, "{dest}->len = {n};");
    if !direct {
        let _ = writeln!(
            out,
            "{} = {};",
            var(r),
            coerce_expr(types, &dest, &list, &target)
        );
    }
}

/// `xs[i]` — bounds-checked read of an element (traps on a bad index), retaining it so the
/// caller owns its own reference.
fn emit_index(
    out: &mut String,
    types: &TypeTable,
    f: &Func,
    result: Option<Value>,
    base: Value,
    index: Value,
    covered: Option<Value>,
) {
    let Some(r) = result else { return };
    let elem = f.value_repr(r).clone();
    let ety = types.c_type(&elem);
    // `m[k]` looks the key up by address and traps when it is absent; `xs[i]` indexes.
    if matches!(f.value_repr(base), Repr::Map(_, _)) {
        let _ = writeln!(
            out,
            "{} = *({ety}*)neon_map_at({}, &{});",
            var(r),
            var(base),
            var(index)
        );
    } else {
        // The width as a literal, so the index multiply folds into an addressing mode.
        // `neon_list_at` reads it from the witness, which stays opaque to the C compiler
        // even fully inlined, and the hot loop of a list walk paid an `imul` per read for
        // it. The element's C type is the one being cast to on this very line, so the
        // literal cannot disagree with the layout.
        match covered {
            // `ir::cover` proved the check redundant whenever this loop-invariant bool
            // is true; gcc unswitches the loop on it and the fast copy carries no check.
            Some(c) => {
                let _ = writeln!(
                    out,
                    "{} = *({ety}*)neon_list_at_scalar_covered({}, {}, sizeof({ety}), {});",
                    var(r),
                    var(base),
                    var(index),
                    var(c)
                );
            }
            None => {
                let _ = writeln!(
                    out,
                    "{} = *({ety}*)neon_list_at_scalar({}, {}, sizeof({ety}));",
                    var(r),
                    var(base),
                    var(index)
                );
            }
        }
    }
    let mut parts = Vec::new();
    rc_parts(types, "neon_retain", &elem, &var(r), &mut parts);
    if !parts.is_empty() {
        let _ = writeln!(out, "{};", parts.join(", "));
    }
}

/// The natives whose element or key crosses the ABI boundary (a witness for construction,
/// a slot pointer for insertion) and so cannot use the plain by-value native call. Maps
/// and `Resource` are here for the same reason lists are, despite the name.
fn is_list_builder(symbol: &str) -> bool {
    matches!(
        symbol,
        "neon_list_new"
            | "neon_list_new_with_capacity"
            | "neon_list_push"
            | "neon_list_set"
            // Emitted by `ir::unique`'s transformation, never by lowering: a write to a
            // list already established as sole-owned, the establishing call itself, and
            // the per-level step of the nested rewrite.
            | "neon_list_set_inplace"
            | "neon_list_ensure_unique"
            | "neon_list_ensure_unique_at"
            | "neon_list_set_field_inplace"
            | "neon_map_new"
            | "neon_map_set"
            | "neon_map_contains"
            | "neon_map_remove"
            | "neon_map_update"
            | "neon_map_keys"
            | "neon_map_values"
            | "neon_resource_new"
            // The channel constructor takes the element witness; a send passes the element
            // by address (a `T | null`-returning recv is statement-shaped and handled
            // before this gate).
            | "neon_channel_new"
            | "neon_channel_new_bounded"
            | "neon_channel_send"
            // A task's result crosses by witness + a per-repr shim, like a spawn argument
            // in the other direction (await is statement-shaped, handled before this gate).
            | "neon_task_lang_spawn"
    )
}

/// The element repr of a channel value: `Channel[T]` is `Repr::Runtime` with `T` as its one
/// argument. Ices on anything else — the checker typed the operand, so a non-channel here is
/// an emitter bug, not a case.
fn chan_elem(types: &TypeTable, f: &Func, v: Value) -> Repr {
    match types.resolve(f.value_repr(v)) {
        Repr::Runtime { args, .. } if args.len() == 1 => args[0].clone(),
        r => ice(r, "channel operation on a non-channel"),
    }
}

/// The per-repr `task::spawn` shim name: calls the zero-arg body and writes the result —
/// whose C type only this emitted code knows — into the task's slot.
fn task_shim_name(types: &TypeTable, t: &Repr) -> String {
    format!("ntask_{}", types.witness_ref(t).trim_start_matches('&'))
}

/// The result repr of a task body: `() -> T`'s `T`, the canonical repr the shim, witness
/// and slot are all keyed by.
fn task_ret(types: &TypeTable, f: &Func, body: Value) -> Repr {
    match types.resolve(f.value_repr(body)) {
        Repr::Closure { params, ret, .. } if params.is_empty() => (**ret).clone(),
        r => ice(r, "task::spawn body is not a zero-argument closure"),
    }
}

/// The cleanup closure's shape: its payload parameter, what it throws, and what it
/// returns.
///
/// Read off the *closure argument* rather than reconstructed from the resource's type.
/// The closure repr already states all three, and it is the thing the emitted drop
/// actually calls — so there is no name to match on, no argument position to assume, and
/// no second place that has to agree with `std::resource`'s signature.
fn cleanup_shape(f: &Func, cleanup: Value) -> Option<(Repr, Repr, Repr)> {
    match f.value_repr(cleanup) {
        Repr::Closure {
            params,
            throws,
            ret,
        } if params.len() == 1 => Some((params[0].clone(), (**throws).clone(), (**ret).clone())),
        _ => None,
    }
}

/// The name of the drop emitted for one cleanup shape.
fn resource_drop_name(types: &TypeTable, t: &Repr, e: &Repr) -> String {
    format!("nres_drop_{}", types.witness_ref(t).trim_start_matches('&'))
        + &format!("_{}", types.witness_ref(e).trim_start_matches('&'))
}

/// The element-wise conversion shim for `List[src] -> List[tgt]`. One per element-repr pair,
/// since that is all the body depends on.
fn list_conv_name(types: &TypeTable, src: &Repr, tgt: &Repr) -> String {
    format!("nlconv_{}", types.witness_ref(src).trim_start_matches('&'))
        + &format!("_{}", types.witness_ref(tgt).trim_start_matches('&'))
}

/// Emit one shim per list conversion the program asked for, and forward-declare them all.
///
/// Runs AFTER the function bodies are emitted, because emitting a body is what requests a
/// conversion. Bodies go to their own buffer so the declarations can still be written above
/// them, and the loop runs to a fixpoint: converting a `List[List[i64]]` coerces its
/// elements, which requests a second shim while the first is being written.
///
/// The element is retained before it is pushed. `neon_list_at` borrows, the source list
/// still owns what it holds, and `neon_list_push` moves the caller's value in -- so without
/// the retain the source's release at the end frees elements the new list is holding.
fn emit_list_convs(out: &mut String, types: &TypeTable) {
    let mut done: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut protos: Vec<String> = Vec::new();
    let mut bodies = String::new();
    loop {
        let maps: Vec<(String, Repr, Repr)> = types
            .map_convs()
            .into_iter()
            .filter(|(n, _, _)| !done.contains(n))
            .collect();
        for (name, sm, tm) in maps {
            done.insert(name.clone());
            protos.push(format!("static neon_map* {name}(neon_map*);"));
            emit_map_conv(&mut bodies, types, &name, &sm, &tm);
        }
        let pending: Vec<(String, Repr, Repr)> = types
            .list_convs()
            .into_iter()
            .filter(|(n, _, _)| !done.contains(n))
            .collect();
        if pending.is_empty() && types.map_convs().iter().all(|(n, _, _)| done.contains(n)) {
            break;
        }
        for (name, se, te) in pending {
            done.insert(name.clone());
            protos.push(format!("static neon_list* {name}(neon_list*);"));
            let sc = types.c_type(&se);
            let tc = types.c_type(&te);
            let w = types.witness_ref(&te);
            let _ = writeln!(bodies, "static neon_list* {name}(neon_list* in) {{");
            // `in->len` rather than `neon_list_len`, which consumes its argument.
            let _ = writeln!(bodies, "    int64_t n = (int64_t)in->len;");
            let _ = writeln!(
                bodies,
                "    neon_list* out = neon_list_new_with_capacity({w}, n);"
            );
            let _ = writeln!(bodies, "    for (int64_t i = 0; i < n; i++) {{");
            let _ = writeln!(bodies, "        {sc} e = *({sc}*)neon_list_at(in, i);");
            let mut parts = Vec::new();
            rc_parts(types, "neon_retain", &se, "e", &mut parts);
            if !parts.is_empty() {
                let _ = writeln!(bodies, "        {};", parts.join(", "));
            }
            let conv = coerce_expr(types, "e", &se, &te);
            let _ = writeln!(bodies, "        {tc} t = {conv};");
            let _ = writeln!(bodies, "        out = neon_list_push(out, &t);");
            let _ = writeln!(bodies, "    }}");
            let _ = writeln!(bodies, "    neon_release((neon_header*)in);");
            let _ = writeln!(bodies, "    return out;");
            let _ = writeln!(bodies, "}}");
        }
    }
    if done.is_empty() {
        return;
    }
    protos.sort();
    for p in &protos {
        let _ = writeln!(out, "{p}");
    }
    out.push('\n');
    out.push_str(&bodies);
    out.push('\n');
}

/// The entry-wise conversion shim for one `Map` widening.
fn map_conv_name(types: &TypeTable, src: &Repr, tgt: &Repr) -> String {
    let part = |r: &Repr| match r {
        Repr::Map(k, v) => format!(
            "{}_{}",
            types.key_witness_ref(k).trim_start_matches('&'),
            types.witness_ref(v).trim_start_matches('&')
        ),
        other => ice(other, "a map conversion whose side is not a map"),
    };
    format!("nmconv_{}__{}", part(src), part(tgt))
}

/// Emit one shim per map conversion, alongside the list ones and to the same fixpoint.
///
/// `neon_map_keys` and `neon_map_values` each CONSUME the map, so one retain up front pays
/// for both calls and leaves `in` fully consumed — which is what the caller expects, since a
/// coercion takes ownership of what it converts.
fn emit_map_conv(bodies: &mut String, types: &TypeTable, name: &str, src: &Repr, tgt: &Repr) {
    let (sk, sv) = match src {
        Repr::Map(k, v) => ((**k).clone(), (**v).clone()),
        other => ice(other, "a map conversion whose source is not a map"),
    };
    let (tk, tv) = match tgt {
        Repr::Map(k, v) => ((**k).clone(), (**v).clone()),
        other => ice(other, "a map conversion whose target is not a map"),
    };
    let (skc, svc) = (types.c_type(&sk), types.c_type(&sv));
    let (tkc, tvc) = (types.c_type(&tk), types.c_type(&tv));
    let _ = writeln!(bodies, "static neon_map* {name}(neon_map* in) {{");
    let _ = writeln!(bodies, "    neon_retain((neon_header*)in);");
    let _ = writeln!(
        bodies,
        "    neon_list* ks = neon_map_keys(in, {});",
        types.witness_ref(&sk)
    );
    let _ = writeln!(
        bodies,
        "    neon_list* vs = neon_map_values(in, {});",
        types.witness_ref(&sv)
    );
    let _ = writeln!(
        bodies,
        "    neon_map* out = neon_map_new({}, {});",
        types.key_witness_ref(&tk),
        types.witness_ref(&tv)
    );
    let _ = writeln!(bodies, "    int64_t n = (int64_t)ks->len;");
    let _ = writeln!(bodies, "    for (int64_t i = 0; i < n; i++) {{");
    let _ = writeln!(bodies, "        {skc} k = *({skc}*)neon_list_at(ks, i);");
    let _ = writeln!(bodies, "        {svc} v = *({svc}*)neon_list_at(vs, i);");
    let mut parts = Vec::new();
    rc_parts(types, "neon_retain", &sk, "k", &mut parts);
    rc_parts(types, "neon_retain", &sv, "v", &mut parts);
    if !parts.is_empty() {
        let _ = writeln!(bodies, "        {};", parts.join(", "));
    }
    let _ = writeln!(
        bodies,
        "        {tkc} k2 = {};",
        coerce_expr(types, "k", &sk, &tk)
    );
    let _ = writeln!(
        bodies,
        "        {tvc} v2 = {};",
        coerce_expr(types, "v", &sv, &tv)
    );
    let _ = writeln!(bodies, "        out = neon_map_set(out, &k2, &v2);");
    let _ = writeln!(bodies, "    }}");
    let _ = writeln!(bodies, "    neon_release((neon_header*)ks);");
    let _ = writeln!(bodies, "    neon_release((neon_header*)vs);");
    let _ = writeln!(bodies, "    return out;");
    let _ = writeln!(bodies, "}}");
}

/// The updater shim for a `map::update` over values of repr `v`. One per value repr, since
/// that is all the shim's body depends on.
fn map_updater_name(types: &TypeTable, v: &Repr) -> String {
    format!("nmap_upd_{}", types.witness_ref(v).trim_start_matches('&'))
}

/// The key and value reprs of a `Map` value.
///
/// Ices rather than answering `(any, any)`. Both reprs are used to pick the witnesses a
/// map hashes and sizes its slots by, and to coerce the key and value crossing the ABI —
/// so guessing `any` here would size every slot as a box and copy the wrong width through
/// a `void*`. Every caller is inside a `neon_map_*` arm whose result is a map.
fn map_kv(f: &Func, v: Value) -> (Repr, Repr) {
    match f.value_repr(v) {
        Repr::Map(k, val) => ((**k).clone(), (**val).clone()),
        other => ice(other, "a map native whose result is not a map"),
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
/// ```text
///     @native("neon_io_read_all") fn read_all(fd: i64) -> (str, i64)
///     // calls: neon_str neon_io_read_all(int64_t fd, int64_t* out_1)
/// ```
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
    let Repr::Tuple(elems) = f.value_repr(r).clone() else {
        return;
    };
    // CORRECT DEFAULT: a native returning the empty tuple has no direct return and no
    // out-parameters, so there is nothing to emit. `()` is `Repr::Unit`, not `Tuple([])`,
    // so this is unreachable in practice as well.
    let Some((first, rest)) = elems.split_first() else {
        return;
    };

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
    let _ = writeln!(
        out,
        "{} = ({}){{ {} }};",
        var(r),
        types.c_type(f.value_repr(r)),
        fields.join(", ")
    );
    let _ = writeln!(out, "}}");
}

/// The codegen-assisted call for each `is_list_builder` symbol.
///
/// These natives cannot use the ordinary by-value path because the runtime is generic over
/// the element type and only the emitter knows it. Two things are therefore supplied here
/// that no Neon signature mentions: the **witness** for the element (its size and its
/// retain/release/eq/cmp), and the element itself **by address** so the container can copy
/// `witness->size` bytes rather than a fixed-width scalar.
///
/// The element reprs come from the *result* value, not from the arguments: the argument
/// may be narrower than the slot it is going into, and `addr_of` coerces it to the
/// container's element repr first. Skipping that step is how a narrow value ends up read
/// at the slot's wider size through a `void*`.
fn emit_list_builder(
    out: &mut String,
    types: &TypeTable,
    f: &Func,
    result: Option<Value>,
    symbol: &str,
    args: &[Value],
) {
    // The in-place write returns nothing — that is its whole point, a call that cannot
    // change the pointer — so its element repr comes from the *list argument*; there is
    // no result to ask. Handled ahead of the result unwrap every other symbol shares.
    // No `is_counted` gate here: `ir::unique` only rewrites writes whose element is
    // uncounted, so reaching this with a refcounted element is a bug in that pass rather
    // than a case to handle. `sizeof` as the width for the same reason `neon_list_set`
    // takes one — the literal cannot disagree with the layout the emitter gave the type.
    // One field of a record element, written straight into the slot. `ir::partial` emits
    // these in place of a whole-record store when the rest of the record is already what is
    // in the slot; its module doc has the safety argument and why it is worth 4.4x -> 2.5x C
    // on n-body.
    //
    // The field's position rides in the third operand as a `const.i64`, so it is read back
    // out of its defining instruction rather than printed as a variable. It names a position
    // in the *element repr's* declared field order, which is the order this emitter lays the
    // struct out in -- so the two cannot drift.
    if let Some(pos) = crate::ir::partial::field_position(f, symbol, args) {
        let Repr::List(e) = f.value_repr(args[0]) else {
            unreachable!("codegen: field write on a non-list")
        };
        let Repr::Record { fields, .. } = types.resolve(e) else {
            unreachable!("codegen: field write on a non-record element")
        };
        let (name, _) = fields[pos].clone();
        let ty = types.c_type(e);
        let _ = writeln!(
            out,
            "((({ty}*)neon_list_at_scalar({}, {}, sizeof({ty})))->{} = {});",
            var(args[0]),
            var(args[1]),
            field_name(&name),
            var(args[3]),
        );
        return;
    }
    // A send reports whether it happened (false = closed; std::channel turns that into
    // ClosedError); its element repr comes from the channel argument. The value crosses
    // by address, coerced to the channel's element repr first — the same rule as a list
    // slot, and for the same reason. Discarding the bool here once left the wrapper's
    // result UNINITIALIZED, which read as "closed" whenever the stack byte was zero.
    if symbol == "neon_channel_send" {
        let e = chan_elem(types, f, args[0]);
        let call = format!(
            "neon_channel_send({}, {})",
            var(args[0]),
            addr_of(types, f, args[1], &e)
        );
        match result {
            Some(r) => {
                let _ = writeln!(out, "{} = {call};", var(r));
            }
            None => {
                let _ = writeln!(out, "{call};");
            }
        }
        return;
    }
    if symbol == "neon_list_set_inplace" {
        let Repr::List(e) = f.value_repr(args[0]) else {
            unreachable!("codegen: in-place write on a non-list")
        };
        let e = (**e).clone();
        let _ = writeln!(
            out,
            "neon_list_set_scalar_inplace({}, {}, {}, sizeof({}));",
            var(args[0]),
            var(args[1]),
            addr_of(types, f, args[2], &e),
            types.c_type(&e)
        );
        return;
    }
    let Some(r) = result else { return };
    // The element repr and its witness are computed *inside* the arms that use them, not
    // once up front. Half the symbols here return a `Map` or a `Resource`, so asking a
    // non-list for its element type is not a corner case, it is the common path — and
    // hoisting it meant `list_elem` had to answer something for a `Map`. It answered `any`,
    // whose witness does not exist, so `witness_ref` returned a null pointer that these
    // arms then discarded unused. Harmless by luck, and it is what stopped both of those
    // functions from being able to refuse.
    let elem = || list_elem(types, f, r);
    let w = || types.witness_ref(&elem());
    let rhs = match symbol {
        // A map's key crosses the boundary by address, like a list element, and its
        // witnesses come from the emitter — the runtime cannot know them.
        "neon_map_new" => {
            let (k, v) = map_kv(f, r);
            format!(
                "neon_map_new({}, {})",
                types.key_witness_ref(&k),
                types.witness_ref(&v)
            )
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
        // Key, fallback and the closure cross by the usual routes; the fifth argument is
        // this instantiation's updater shim -- the only code that knows `V`'s C type and so
        // the only code that can perform the call.
        "neon_map_update" => {
            let (k, v) = map_kv(f, r);
            format!(
                "neon_map_update({}, {}, {}, {}, {})",
                var(args[0]),
                addr_of(types, f, args[1], &k),
                addr_of(types, f, args[2], &v),
                var(args[3]),
                map_updater_name(types, &v)
            )
        }
        "neon_map_contains" => format!("neon_map_contains({}, &{})", var(args[0]), var(args[1])),
        "neon_map_remove" => {
            // The key crosses by address and must be coerced to the map's key repr first,
            // exactly as `set` does -- a narrower argument would otherwise be read at the
            // wrong width through the void pointer.
            let (k, _) = map_kv(f, r);
            format!(
                "neon_map_remove({}, {})",
                var(args[0]),
                addr_of(types, f, args[1], &k)
            )
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
        // The channel stores its element witness at construction, exactly as a list does.
        "neon_channel_new" => {
            let e = chan_elem(types, f, r);
            format!("neon_channel_new({})", types.witness_ref(&e))
        }
        "neon_channel_new_bounded" => {
            let e = chan_elem(types, f, r);
            format!(
                "neon_channel_new_bounded({}, {})",
                types.witness_ref(&e),
                var(args[0])
            )
        }
        // The task carries its RESULT's witness and per-repr call shim; both are keyed by
        // the body closure's return repr, which only this emitter knows.
        "neon_task_lang_spawn" => {
            let t = task_ret(types, f, args[0]);
            format!(
                "neon_task_lang_spawn({}, {}, {})",
                var(args[0]),
                types.witness_ref(&t),
                task_shim_name(types, &t)
            )
        }
        "neon_list_new" => format!("neon_list_new({})", w()),
        "neon_list_new_with_capacity" => {
            format!("neon_list_new_with_capacity({}, {})", w(), var(args[0]))
        }
        // The element is passed by address; the list moves its bytes in through the witness.
        "neon_list_push" => {
            format!(
                "neon_list_push({}, {})",
                var(args[0]),
                addr_of(types, f, args[1], &elem())
            )
        }
        // An element type that is not refcounted takes the specialised setter: the slot
        // being overwritten needs no release, and `sizeof` is a constant here, so the copy
        // folds to a store instead of a witness-sized `memcpy`. The generic version has to
        // read both facts off the witness at run time and can do neither. Worth 15% on the
        // brainfuck benchmark's write loop; see `neon_list_set_scalar`.
        //
        // `is_counted` is the precondition that function states, asked of the repr codegen
        // already has. `sizeof` rather than a size computed here, so the literal cannot
        // disagree with the layout the emitter actually gave the type.
        "neon_list_ensure_unique" => format!("neon_list_ensure_unique({})", var(args[0])),
        // The per-level step of `ir::unique`'s nested rewrite: plain pointer arguments,
        // no witness — the slots of a `List[List[..]]` are `neon_list*` by layout.
        "neon_list_ensure_unique_at" => format!(
            "neon_list_ensure_unique_at({}, {})",
            var(args[0]),
            var(args[1])
        ),
        "neon_list_set" if !elem().is_counted() => format!(
            "neon_list_set_scalar({}, {}, {}, sizeof({}))",
            var(args[0]),
            var(args[1]),
            addr_of(types, f, args[2], &elem()),
            types.c_type(&elem())
        ),
        "neon_list_set" => format!(
            "neon_list_set({}, {}, {})",
            var(args[0]),
            var(args[1]),
            addr_of(types, f, args[2], &elem())
        ),
        // `is_list_builder` is the only gate on reaching here, so the two lists must
        // agree; a symbol added to one and not the other would otherwise emit nothing.
        other => unreachable!("codegen: `{other}` is an is_list_builder with no emission"),
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
                let Op::Native { symbol, .. } = &inst.op else {
                    continue;
                };
                if symbol != "neon_resource_new" {
                    continue;
                }
                let Op::Native { args, .. } = &inst.op else {
                    continue;
                };
                // CORRECT DEFAULT here, and checked elsewhere: skipping emits no drop for
                // this instantiation, but `emit_list_builder` reaches the same
                // `cleanup_shape` for the same instruction and `expect`s it, so a `None`
                // fails there with a message rather than silently shipping a resource whose
                // cleanup never runs.
                let Some((t, e, ret)) = cleanup_shape(f, args[1]) else {
                    continue;
                };
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
        let retc = if throws {
            types.c_type(&tagged)
        } else {
            "void".to_string()
        };
        // This is a REF drop: it fires for every ref of the instantiation, owning or not.
        // Only the OWNING ref's death runs cleanup — that is the baton — and `take`
        // disarms first, so the two are exactly-once together. The zeroing inside `take`
        // still matters: the closure consumes the payload, and the body's own drop
        // releases whatever is left in the slot, so bytes left behind are released twice.
        let _ = writeln!(out, "static void {name}(void* p) {{");
        let _ = writeln!(out, "    neon_resource* r = (neon_resource*)p;");
        let _ = writeln!(out, "    {tc} pay;");
        let _ = writeln!(
            out,
            "    if (r->owning && neon_resource_take(r->body, &pay)) {{"
        );
        let call = format!(
            "(({retc}(*)(neon_header*, {tc}))r->body->cleanup.fn)(r->body->cleanup.env, pay)"
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
        let _ = writeln!(out, "    neon_resource_ref_finish(r);");
        let _ = writeln!(out, "}}");
    }
    out.push('\n');
}

/// Emit the updater shim `neon_map_update` calls back through, one per value repr in the
/// program's `map::update` instantiations.
///
/// The runtime holds the value only as bytes behind a `void*`, so it cannot perform the
/// `(V) -> V` call itself — the C signature of that call is a function of `V`. This shim is
/// the piece that knows `V`, exactly as `nres_drop_*` is for a resource's cleanup.
///
/// Reading `in` into a local before storing to `out` is not incidental: `update` passes the
/// same slot as both on the key-present path, and `neon_map_updater` documents that the
/// read must complete first.
fn emit_map_updaters(out: &mut String, types: &TypeTable, program: &Program) {
    let mut seen: std::collections::BTreeMap<String, Repr> = std::collections::BTreeMap::new();
    for f in &program.funcs {
        for b in &f.blocks {
            for inst in &b.insts {
                let Op::Native { symbol, .. } = &inst.op else {
                    continue;
                };
                if symbol != "neon_map_update" {
                    continue;
                }
                // The result is the updated map, so its value repr is the one the closure
                // maps over. `map_kv` ices on a non-map, which is the same guarantee the
                // call arm relies on.
                let Some(r) = inst.result else { continue };
                let (_, v) = map_kv(f, r);
                seen.insert(map_updater_name(types, &v), v);
            }
        }
    }
    if seen.is_empty() {
        return;
    }
    for (name, v) in seen {
        let vc = types.c_type(&v);
        let _ = writeln!(
            out,
            "static void {name}(neon_closure f, const void* in, void* out) {{ {vc} v = *(const {vc}*)in; *({vc}*)out = (({vc}(*)(neon_header*, {vc}))f.fn)(f.env, v); }}",
        );
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
        // CORRECT DEFAULT: `targets` is collected from `MakeClosure` ops, whose `func`
        // always names a function in this program, so the miss cannot happen. If it did,
        // the omitted thunk is an undefined symbol at link time — never a silent answer.
        let Some(target) = by_name.get(name) else {
            continue;
        };
        let params: Vec<String> = target
            .params
            .iter()
            .enumerate()
            .map(|(i, &p)| format!("{} _a{i}", types.c_type(target.value_repr(p))))
            .collect();
        let args: Vec<String> = (0..target.params.len()).map(|i| format!("_a{i}")).collect();
        let sig_params = std::iter::once("neon_header* _env".to_string())
            .chain(params)
            .collect::<Vec<_>>()
            .join(", ");
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
fn emit_make_closure(
    out: &mut String,
    types: &TypeTable,
    f: &Func,
    result: Option<Value>,
    func: &str,
    captures: &[Value],
) {
    let Some(r) = result else { return };
    // A lambda already has the `(env, args…)` shape; an ordinary function used as a value
    // is reached through its adapter thunk.
    let target = if types.is_lambda(func) {
        mangle(func)
    } else {
        thunk_name(func)
    };
    let fnptr = format!("(void*){target}");
    if captures.is_empty() {
        let _ = writeln!(
            out,
            "{} = (neon_closure){{ {fnptr}, (neon_header*)0 }};",
            var(r)
        );
        return;
    }
    let env = Repr::Tuple(captures.iter().map(|&c| f.value_repr(c).clone()).collect());
    let ty = types.c_type(&env);
    let drop = types.env_drop_ref(&env, types.env_move(func));
    let inits: Vec<String> = captures
        .iter()
        .enumerate()
        .map(|(i, &c)| format!("._{i} = {}", var(c)))
        .collect();
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
    for (name, repr, _mv) in types.env_drops() {
        let ty = types.c_type(repr);
        let mut parts = Vec::new();
        rc_parts(types, "neon_release", repr, "(*e)", &mut parts);
        let releases = if parts.is_empty() {
            String::new()
        } else {
            format!("{}; ", parts.join("; "))
        };
        let _ = writeln!(
            out,
            "static void {name}(void* p) {{ neon_header* h = (neon_header*)p; {ty}* e = ({ty}*)(h + 1); {releases}neon_free(h); }}",
        );
    }
    if !types.env_drops().is_empty() {
        out.push('\n');
    }
}

/// Emit a deep copy per closure-environment shape, plus the table mapping each shape's
/// DROP function to its copy — the runtime's only handle on a closure's type at spawn time
/// is `env->drop`, and this table is what turns it back into "how to copy these captures".
/// The same drop-pointer-as-type-id trick the teardown walk leans on. Fiber programs only;
/// the table symbol is weak-defaulted empty in the runtime for the C test binary, and a
/// program that never touches fibers neither emits nor links any of it.
fn emit_env_copies(out: &mut String, types: &TypeTable) {
    if !types.fiber_program {
        return;
    }
    let drops = types.env_drops();
    for (name, repr, mv) in drops {
        let ty = types.c_type(repr);
        let mut stmts = Vec::new();
        copy_stmts(types, repr, "(*s)", "(*d)", &mut stmts);
        let over = if stmts.is_empty() {
            String::new()
        } else {
            format!(" {};", stmts.join("; "))
        };
        // A `move` lambda's env copy IS a transfer edge: the whole overwrite runs inside
        // the bracket, so any owning resource ref among the captures (directly or behind
        // `Any`) hands its baton to the copy that rides to the new fiber. A plain env's
        // copy mints non-owning refs — the runtime backstop for what the static
        // capture check cannot see.
        let (tb, te) = if *mv {
            (" neon_transfer_begin();", " neon_transfer_end();")
        } else {
            ("", "")
        };
        // Allocate with the SAME drop the closure constructor uses, bitcopy, then overwrite
        // heap parts with independent copies — the boxed-record copy's shape exactly.
        let _ = writeln!(
            out,
            "static neon_header* {name}_ecopy(const neon_header* src) {{ \
             {ty} const* s = ({ty} const*)(src + 1); \
             neon_header* h = neon_alloc(sizeof({ty}), {name}); \
             {ty}* d = ({ty}*)(h + 1); *d = *s;{tb}{over}{te} return h; }}",
        );
    }
    let entries: Vec<String> = drops
        .iter()
        .map(|(name, _, _)| format!("{{ {name}, {name}_ecopy }}"))
        .collect();
    // Strong definitions overriding the runtime's weak empty defaults. Pointer + count,
    // never an extern array: the types must be identical on both sides for the linker (and
    // LTO in particular) to interpose cleanly.
    if entries.is_empty() {
        let _ = writeln!(
            out,
            "const neon_env_copy_entry* neon_env_copy_entries = 0;\n\
             size_t neon_env_copy_count = 0;\n"
        );
    } else {
        let _ = writeln!(
            out,
            "static const neon_env_copy_entry neon_env_copies_[] = {{ {} }};\n\
             const neon_env_copy_entry* neon_env_copy_entries = neon_env_copies_;\n\
             size_t neon_env_copy_count = {};\n",
            entries.join(", "),
            entries.len()
        );
    }
}

/// Emit a key-witness — hash and content equality — for each type used as a map key.
fn emit_key_witnesses(out: &mut String, types: &TypeTable) {
    for (name, repr) in types.key_witnesses() {
        let ty = types.c_type(repr);
        let _ = writeln!(
            out,
            "static uint64_t {name}_hash(const void* p) {{ {ty} const* e = ({ty} const*)p; return {}; }}",
            hash_expr(types, repr, "(*e)"),
        );
        let _ = writeln!(
            out,
            "static bool {name}_eq(const void* pa, const void* pb) {{ {ty} const* a = ({ty} const*)pa; {ty} const* b = ({ty} const*)pb; return {}; }}",
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
///
/// **The invariant this function exists to keep is `eq_expr(a, b) => hash(a) == hash(b)`.**
/// Break it and the key is simply not found: it hashes to a bucket nobody looks in, and
/// the map answers `false` to a `contains` for a key it holds. There is no crash and no
/// diagnostic, which is why every arm below is spelled out and the match has no catch-all.
///
/// The catch-all it replaces hashed the raw representation, and was reached by every shape
/// `eq_expr` compares *structurally* through a pointer or a tag. `Map[str | null, V]` was
/// the smallest case: `nkw0_eq` called `neon_str_eq` while `nkw0_hash` hashed the
/// `{data, len, owner}` triple, so two equal strings at different addresses compared equal
/// and hashed differently, and a 40-entry map found none of them.
///
/// Weak-but-correct beats wrong: where no content hash is expressible here (a `List`, a
/// `Map`, a self-referencing record) this hashes a length or a constant, which collides
/// more but always lets `eq` decide. Only a *disagreement* loses data.
fn hash_expr(types: &TypeTable, r: &Repr, e: &str) -> String {
    // Bound rather than matched on, for the same reason `eq_expr` binds it: the `Nullable`
    // arm passes `r` to `null_test`, which must see the resolved shape.
    let r = types.resolve(r);
    match r {
        Repr::Str => format!("neon_hash_bytes({e}.data, {e}.len)"),
        Repr::Record { fields, .. } => fields
            .iter()
            .map(|(n, fr)| hash_expr(types, fr, &format!("{e}.{}", field_name(n))))
            .reduce(|a, b| format!("neon_hash_mix({a}, {b})"))
            // CORRECT DEFAULT: a fieldless record has no content to mix, so every value of
            // it hashes alike — which is what `eq_expr` requires, since it answers `true`
            // for any two of them.
            .unwrap_or_else(|| "0".into()),
        Repr::Tuple(elems) => elems
            .iter()
            .enumerate()
            .map(|(i, er)| hash_expr(types, er, &format!("{e}._{i}")))
            .reduce(|a, b| format!("neon_hash_mix({a}, {b})"))
            .unwrap_or_else(|| "0".into()),
        // By length, not by address: `eq_expr` compares lists elementwise, and equal keys
        // must hash equal. Hashing the elements too would need a loop, so a per-element
        // hash on the value-witness -- the pointer the layering deliberately keeps off it.
        // Length alone is a correct hash, just a weak one: `Map[List[T], V]` keyed on
        // same-length lists degrades toward a linear probe, and buying more than that
        // means giving every element type a hash function.
        Repr::List(_) => format!("neon_hash_bytes(&{e}->len, sizeof {e}->len)"),
        // Same bargain as `List`, for the same reason: `eq_expr` calls `neon_map_eq`, which
        // is content equality, so the address must not enter the hash. Entry count is the
        // only content-derived number reachable from an expression.
        Repr::Map(_, _) => format!("neon_hash_bytes(&{e}->len, sizeof {e}->len)"),
        // `eq_expr` walks a self-referencing record through a generated recursive function;
        // an expression cannot follow that walk, so every such key hashes alike and `eq`
        // decides. Degenerate (a linear scan) but never wrong -- unlike hashing the
        // pointer, which disagreed with an `eq` that reads through it.
        Repr::BoxedRec(_) => "0".to_string(),
        // Null-ness first, exactly as `eq_expr` tests it: all nulls share one hash, and a
        // present payload hashes as itself. Hashing the pointer-or-`neon_str` whole is what
        // broke `Map[str | null, V]`.
        Repr::Nullable(inner) => {
            format!(
                "({} ? 0ULL : {})",
                null_test(r, e),
                hash_expr(types, inner, e)
            )
        }
        // Tag first, then the payload that tag selects -- mirroring `eq_expr`, which
        // requires equal tags. Hashing the payload union whole read the bytes past the live
        // variant, which are never written.
        Repr::Union(variants) => {
            variants
                .iter()
                .enumerate()
                .rev()
                .fold("0ULL".to_string(), |rest, (i, v)| {
                    let h = hash_expr(types, v, &format!("{e}.u._{i}"));
                    format!("({e}.tag == {i} ? neon_hash_mix({i}ULL, {h}) : {rest})")
                })
        }
        // One inhabitant, so `eq_expr` answers `true` without reading anything and the hash
        // must be a constant too. A `neon_unit` in a union payload is never written, so
        // hashing its byte would hash uninitialised memory -- a key that does not reliably
        // hash to the same bucket as itself.
        Repr::Unit | Repr::Null => "0ULL".to_string(),
        // Flat scalars, compared with `==`. `f64` inherits IEEE's two quirks here: `-0.0`
        // and `+0.0` are `==` but hash differently, and a NaN key is never equal to itself.
        // Both follow from structural comparison at the leaf (docs/decisions.md).
        Repr::I64 | Repr::F64 | Repr::Bool | Repr::Tag => {
            format!("neon_hash_bytes(&{e}, sizeof {e})")
        }
        // Identity, matching an `eq_expr` that is also identity: two handles are the same
        // handle or they are not, and an erased value's box is compared by address.
        Repr::Runtime { .. } | Repr::Any | Repr::Closure { .. } => {
            format!("neon_hash_bytes(&{e}, sizeof {e})")
        }
        // Uninhabited: emitted as the dead arm of a union's tag chain, never evaluated.
        Repr::Never => "0ULL".to_string(),
        other => ice(other, "hashing a map key"),
    }
}

/// Whether `cmp_expr` can order this repr — the backend's half of the checker's
/// `is_ordered`. A union has no order (ranking its arms would be an invention), and a
/// closure, map or boxed recursive record has none either; an aggregate is ordered exactly
/// when every part of it is.
fn has_order(r: &Repr) -> bool {
    match r {
        Repr::I64 | Repr::F64 | Repr::Bool | Repr::Tag | Repr::Str | Repr::Unit | Repr::Null => {
            true
        }
        Repr::Record { fields, .. } => fields.iter().all(|(_, fr)| has_order(fr)),
        Repr::Tuple(elems) => elems.iter().all(has_order),
        Repr::List(e) => has_order(e),
        _ => false,
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
///
/// The final arm panics rather than falling back: `has_order` decides which reprs may
/// reach here, and a repr arriving that it excludes means the two have drifted apart.
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
            .fold("0".to_string(), |rest, (fr, fa, fb)| {
                lex_then(types, fr, &fa, &fb, &rest)
            }),
        Repr::Tuple(elems) => elems
            .iter()
            .enumerate()
            .map(|(i, er)| (er, format!("{a}._{i}"), format!("{b}._{i}")))
            .rev()
            .fold("0".to_string(), |rest, (er, ea, eb)| {
                lex_then(types, er, &ea, &eb, &rest)
            }),
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
    format!(
        "({} ? {rest} : {})",
        eq_expr(types, r, a, b),
        cmp_expr(types, r, a, b)
    )
}

/// Whether an expression of nullable repr `r` holds null, as a C condition. Mirrors
/// `is_null`, which answers the same question for a `Value` rather than an expression.
fn null_test(r: &Repr, e: &str) -> String {
    // The catch-all is a pointer test, which is right for every nullable whose payload is
    // pointer-backed — a list, a map, a boxed record, a runtime handle. `str` and closures
    // are the two that are *not* bare pointers and so get their own arms above. A payload
    // that is neither (an `i64 | null` is a `Repr::Union`, never a `Nullable`) would emit
    // `struct == NULL`, which `cc` rejects: a loud failure, not a wrong answer.
    match r {
        Repr::Nullable(inner) if matches!(inner.as_ref(), Repr::Str) => {
            format!("({e}.data == NULL)")
        }
        // A closure is a `{fn, env}` pair: a capture-less closure has a NULL env and is
        // not null, so nullability rides on the function pointer.
        Repr::Nullable(inner) if matches!(inner.as_ref(), Repr::Closure { .. }) => {
            format!("({e}.fn == NULL)")
        }
        _ => format!("({e} == NULL)"),
    }
}

/// Content equality, matching `hash_expr`: equal keys must hash equal.
///
/// Exhaustive, and deliberately so. The `memcmp` catch-all this replaces was correct only
/// for a repr with no padding and no indirection, and every arm added below was previously
/// reached by it: a `Recursive` back-edge (never resolved, so a `mu` type compared as raw
/// bytes), a `Closure`, an erased `any`, and the uninhabited error half of every
/// non-throwing function's tagged result.
fn eq_expr(types: &TypeTable, r: &Repr, a: &str, b: &str) -> String {
    // Bound, not just matched on: the `Nullable` arm hands `r` to `null_test`, and an
    // unresolved back-edge there missed the `Nullable(Closure)` case and emitted
    // `neon_closure == NULL`, which C rejects outright.
    let r = types.resolve(r);
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
        // Identity, not contents: two handles are the same handle or they are not.
        // The per-type entry that a uniform `Runtime` repr cannot derive.
        Repr::Runtime { .. } => format!("({a} == {b})"),
        // A self-referencing record is a pointer, and comparing it means walking through
        // that pointer -- which a nested expression cannot do, since the walk recurses.
        // `emit_boxed_eq` generates one function per boxed record for exactly this.
        // A `false` here would be a plausible-looking answer to "are these two records
        // equal" for a record whose wrapper was never registered -- so it ices instead.
        // `emit_boxed_eq` generates one `_eq` per entry in the same table, so a miss means
        // the two walked different tables.
        boxed @ Repr::BoxedRec(_) => match types.boxed_shape(boxed) {
            Some((name, _)) => format!("{name}_eq({a}, {b})"),
            None => ice(boxed, "comparing a boxed record with no registered wrapper"),
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
            let arms =
                variants
                    .iter()
                    .enumerate()
                    .rev()
                    .fold("true".to_string(), |rest, (i, v)| {
                        let (pa, pb) = (format!("{a}.u._{i}"), format!("{b}.u._{i}"));
                        format!(
                            "({a}.tag == {i} ? {} : {rest})",
                            eq_expr(types, v, &pa, &pb)
                        )
                    });
            format!("({a}.tag == {b}.tag && {arms})")
        }
        // No structural answer exists for a function, and `is_equatable` rejects `==` on
        // one for exactly that reason -- but a value-witness is emitted for *every*
        // element repr, used or not, so `List[fn]` still needs an `eq` that compiles.
        // Identity is what that one means: the same `{fn, env}` pair or not.
        Repr::Closure { .. } => {
            format!("(({a}.fn == {b}.fn) && ({a}.env == {b}.env))")
        }
        // Same shape: `is_equatable` rejects `==` on `any`, and this exists only so a
        // container of `any` has a witness. Box identity, matching `hash_expr`. Content
        // equality through a box would need the runtime to dispatch on the stored tag.
        Repr::Any => format!("({a} == {b})"),
        // Uninhabited. This is emitted, and often: it is the error half of a *non*-throwing
        // function's tagged result, so the union arm above walks it and a C expression has
        // to appear. `true` is the answer for a variant no value can hold, and the tag test
        // guarding it is never satisfied.
        Repr::Never => "true".to_string(),
        // A back-edge is resolved at the top of this match; reaching here means it names a
        // type the table does not hold, and comparing it would compare a pointer to a
        // structure this function is meant to walk.
        other => ice(other, "structural equality"),
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
        let _ = writeln!(
            out,
            "static bool {name}_eq(const {name}* a, const {name}* b);"
        );
    }
    for (name, shape) in &boxed {
        let _ = writeln!(
            out,
            "static bool {name}_eq(const {name}* a, const {name}* b) {{"
        );
        // The same pointer is the same value; one null and one not cannot be equal. A
        // boxed field that is `T | null` arrives here as a null pointer.
        let _ = writeln!(out, "if (a == b) return true;");
        let _ = writeln!(out, "if (a == NULL || b == NULL) return false;");
        // The record sits behind the header in a `value` member, not inline.
        let _ = writeln!(
            out,
            "return {};",
            eq_expr(types, shape, "(a->value)", "(b->value)")
        );
        let _ = writeln!(out, "}}");
    }
    if !boxed.is_empty() {
        out.push('\n');
    }
}

/// Emit a value-witness for each container element type: its size, in-place retain and
/// release, structural equality, and — only when `has_order` admits the repr — a compare.
/// A null function pointer stands for "nothing to do": an element with no counted parts
/// gets no retain or release, and an unordered one gets no `cmp`, so the runtime skips the
/// work instead of calling a function that does nothing.
/// The deep-copy statements for one repr, `src` into `dst` (both C lvalue expressions of
/// the repr's C type, `dst` raw non-owning memory already holding a bitcopy of `src`).
/// Scalars need nothing — the bitcopy was the copy; heap parts are overwritten with
/// independent copies through the runtime `neon_wcopy_*` helpers (NULL-tolerant for the
/// pointer reprs a nullable shares its representation with) and the per-boxed-record copy
/// functions. A part that cannot cross fibers (a closure, a non-channel runtime handle)
/// emits the trapping helper — loudness over corruption, the witness `copy` contract.
///
/// Terminates on recursive types for the same reason `emit_boxed_eq`'s walk does: every
/// cycle passes through a `BoxedRec`, which emits a CALL to its per-type function rather
/// than inlining.
fn copy_stmts(types: &TypeTable, repr: &Repr, src: &str, dst: &str, out: &mut Vec<String>) {
    if types.is_boxed(repr) {
        let (wrapper, _) = types
            .boxed_shape(repr)
            .expect("copying a boxed record with no registered wrapper");
        out.push(format!("{dst} = {wrapper}_copy({src})"));
        return;
    }
    match types.resolve(repr).clone() {
        Repr::Str => out.push(format!("neon_wcopy_str(&{src}, &{dst})")),
        Repr::List(_) => out.push(format!("neon_wcopy_list(&{src}, &{dst})")),
        Repr::Map(_, _) => out.push(format!("neon_wcopy_map(&{src}, &{dst})")),
        Repr::Any => out.push(format!("neon_wcopy_any(&{src}, &{dst})")),
        Repr::Runtime { c_type, .. } if c_type == "neon_channel_ref" => {
            out.push(format!("neon_wcopy_channel(&{src}, &{dst})"))
        }
        Repr::Runtime { c_type, .. } if c_type == "neon_task_ref" => {
            out.push(format!("neon_wcopy_task(&{src}, &{dst})"))
        }
        // A CAPTURE-FREE closure is a plain function pointer and crosses fine — that is what
        // lets a named handler be handed to a spawned fiber. A capturing one traps, because
        // its environment would have to be deep-copied and that is the env-copy table's job
        // at a spawn boundary, not a value copy's. The test is on the value, so the runtime
        // helper makes it.
        Repr::Closure { .. } => out.push(format!("neon_wcopy_closure(&{src}, &{dst})")),
        // A resource crossing mints a fresh REF to the same body — inside a transfer
        // bracket the owning ref's copy moves the baton (runtime/src/resource.c). This is
        // what made `serve` a loop any user can write.
        Repr::Runtime { c_type, .. } if c_type == "neon_resource" => {
            out.push(format!("neon_wcopy_resource(&{src}, &{dst})"))
        }
        Repr::Runtime { .. } => {
            out.push("neon_wcopy_unsendable((const void*)0, (void*)0)".to_string())
        }
        Repr::Union(variants) => {
            let mut arms = String::new();
            for (i, v) in variants.iter().enumerate() {
                let mut sub = Vec::new();
                copy_stmts(
                    types,
                    v,
                    &format!("{src}.u._{i}"),
                    &format!("{dst}.u._{i}"),
                    &mut sub,
                );
                if !sub.is_empty() {
                    let _ = write!(arms, "case {i}: {}; break; ", sub.join("; "));
                }
            }
            if !arms.is_empty() {
                out.push(format!("switch ({src}.tag) {{ {arms}default: break; }}"));
            }
        }
        Repr::Nullable(inner) => copy_stmts(types, &inner, src, dst, out),
        Repr::Record { fields, .. } => {
            for (n, fr) in &fields {
                let fname = field_name(n);
                copy_stmts(
                    types,
                    fr,
                    &format!("{src}.{fname}"),
                    &format!("{dst}.{fname}"),
                    out,
                );
            }
        }
        Repr::Tuple(elems) => {
            for (i, e) in elems.iter().enumerate() {
                copy_stmts(
                    types,
                    e,
                    &format!("{src}._{i}"),
                    &format!("{dst}._{i}"),
                    out,
                );
            }
        }
        Repr::I64
        | Repr::F64
        | Repr::Bool
        | Repr::Tag
        | Repr::Unit
        | Repr::Null
        | Repr::Never
        | Repr::BoxedRec(_)
        | Repr::Recursive(_) => {}
        r @ Repr::Var(_) => ice(&r, "copying a type variable"),
    }
}

/// The witness `copy` member for a repr: `0` when a bitcopy is independent (scalar-only),
/// a runtime helper where one repr IS the helper's shape, or a generated wrapper holding
/// the composite walk. Returns the C expression for the table literal.
fn witness_copy_ref(out: &mut String, types: &TypeTable, name: &str, repr: &Repr) -> String {
    match types.resolve(repr) {
        Repr::Str => return "neon_wcopy_str".into(),
        Repr::List(_) => return "neon_wcopy_list".into(),
        Repr::Map(_, _) => return "neon_wcopy_map".into(),
        Repr::Any => return "neon_wcopy_any".into(),
        Repr::Runtime { c_type, .. } if c_type == "neon_channel_ref" => {
            return "neon_wcopy_channel".into()
        }
        Repr::Runtime { c_type, .. } if c_type == "neon_task_ref" => {
            return "neon_wcopy_task".into()
        }
        Repr::Closure { .. } => return "neon_wcopy_closure".into(),
        Repr::Runtime { c_type, .. } if c_type == "neon_resource" => {
            return "neon_wcopy_resource".into()
        }
        Repr::Runtime { .. } => return "neon_wcopy_unsendable".into(),
        _ => {}
    }
    let mut stmts = Vec::new();
    copy_stmts(types, repr, "(*s)", "(*d)", &mut stmts);
    if stmts.is_empty() {
        return "0".into();
    }
    let ty = types.c_type(repr);
    let fname = format!("{name}_copy");
    let _ = writeln!(
        out,
        "static void {fname}(const void* ps, void* pd) {{ {ty} const* s = ({ty} const*)ps; {ty}* d = ({ty}*)pd; *d = *s; {}; }}",
        stmts.join("; ")
    );
    fname
}

/// Forward-declare the per-boxed-record deep copies, before the witness wrappers that call
/// them; the definitions follow the env drops (whose symbols they allocate with).
fn emit_boxed_copy_decls(out: &mut String, types: &TypeTable) {
    let boxed = types.boxed_records();
    for (name, _) in &boxed {
        let _ = writeln!(out, "static {name}* {name}_copy(const {name}* s);");
    }
    if !boxed.is_empty() {
        out.push('\n');
    }
}

/// The deep copy for each boxed (self-referencing) record: allocate a fresh wrapper with
/// the SAME drop the constructor uses (so releasing the copy walks the same shape), bitcopy
/// the value, then overwrite its heap parts with independent copies. NULL passes through —
/// a boxed field that is `T | null` arrives as a null pointer. Mutual recursion works for
/// the same reason `emit_boxed_eq`'s does: everything is forward-declared, and the value
/// graph is a DAG (records are immutable), so the walk terminates.
fn emit_boxed_copy_defs(out: &mut String, types: &TypeTable) {
    let boxed = types.boxed_records();
    for (name, shape) in &boxed {
        let sty = types.c_type(shape);
        let mut stmts = Vec::new();
        copy_stmts(types, shape, "(s->value)", "(d->value)", &mut stmts);
        let body = if stmts.is_empty() {
            String::new()
        } else {
            format!(" {};", stmts.join("; "))
        };
        let _ = writeln!(
            out,
            "static {name}* {name}_copy(const {name}* s) {{ if (s == NULL) return NULL; \
             {name}* d = ({name}*)neon_alloc(sizeof({sty}), {}); d->value = s->value;{body} return d; }}",
            types.env_drop_ref(shape, false)
        );
    }
    if !boxed.is_empty() {
        out.push('\n');
    }
}

/// One `task::spawn` call shim per RESULT repr in the program: call the zero-argument body
/// closure and write its result — whose C type only this emitted code knows — into the
/// task's slot. The map-updater pattern, for a task.
fn emit_spawn_shims(out: &mut String, types: &TypeTable, program: &Program) {
    let mut rets_seen: std::collections::BTreeMap<String, Repr> = std::collections::BTreeMap::new();
    for f in &program.funcs {
        for b in &f.blocks {
            for inst in &b.insts {
                let Op::Native { symbol, args } = &inst.op else {
                    continue;
                };
                if symbol != "neon_task_lang_spawn" {
                    continue;
                }
                let t = task_ret(types, f, args[0]);
                rets_seen.insert(task_shim_name(types, &t), t);
            }
        }
    }
    for (name, t) in &rets_seen {
        let tc = types.c_type(t);
        let _ = writeln!(
            out,
            "static void {name}(neon_closure f, void* out) {{ *({tc}*)out = (({tc}(*)(neon_header*))f.fn)(f.env); }}",
        );
    }
    if !rets_seen.is_empty() {
        out.push('\n');
    }
}

fn emit_witnesses(out: &mut String, types: &TypeTable) {
    for (name, repr) in types.witnesses() {
        let ty = types.c_type(repr);
        let retain = emit_witness_fn(out, types, name, repr, "retain", "neon_retain");
        let release = emit_witness_fn(out, types, name, repr, "release", "neon_release");
        let copy = witness_copy_ref(out, types, name, repr);
        // Structural comparison, so `==` and `<` on a list can walk its elements. `eq`
        // always exists; `cmp` only for an element that has an order.
        // `{ty} const*`, not `const {ty}*`. The two are the same for a flat repr, but a
        // pointer repr (`neon_map*`, `neon_list*`) is one where they differ: the prefix form
        // binds `const` to the map, not to the slot, and the element then cannot be passed to
        // a runtime native that takes it unqualified. What is read-only here is the *slot*
        // the witness was handed, so the qualifier belongs on the outer pointer.
        let cast = format!("{ty} const* a = ({ty} const*)pa; {ty} const* b = ({ty} const*)pb;");
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
            "static const neon_witness {name} = {{ sizeof({ty}), {retain}, {release}, {name}_eq, {cmp}, {copy} }};"
        );
    }
    if !types.witnesses().is_empty() {
        out.push('\n');
    }
}

/// Emit one witness function (retain or release) if the element has counted parts, and
/// return its name; otherwise emit nothing and return `0` (a null function pointer).
fn emit_witness_fn(
    out: &mut String,
    types: &TypeTable,
    name: &str,
    repr: &Repr,
    which: &str,
    func: &str,
) -> String {
    let mut parts = Vec::new();
    rc_parts(types, func, repr, "(*e)", &mut parts);
    if parts.is_empty() {
        return "0".into();
    }
    let ty = types.c_type(repr);
    let fname = format!("{name}_{which}");
    let _ = writeln!(
        out,
        "static void {fname}(void* p) {{ {ty}* e = ({ty}*)p; {}; }}",
        parts.join("; ")
    );
    fname
}

/// A block's terminator.
///
/// The `throws` arms come first and are what makes the calling convention real: in a
/// throwing function *both* `ret` and `throw` are C `return` statements, differing only in
/// which tag of the result union they inject into. A `throw` in a function that declares
/// no `throws` has no such union to return through, so it can only panic — that is an
/// error escaping `main`, not an unhandled case.
fn emit_term(out: &mut String, types: &TypeTable, f: &Func, cur: u32, term: &Term) {
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
        // Unreachable by construction: the one `Term::Throw` constructor
        // (`lower.rs::throw_or_escape`) only emits it when the function has a throws
        // slot, and that shape is the guarded arm above. A throw with no handler and
        // no throws clause is stringified there (`error_message`) and panics through
        // `Op::Native` instead — `neon_panic` takes a `neon_str`, and the raw error
        // value this arm used to pass it was never the right type.
        Term::Throw(_) => unreachable!(
            "codegen: `Term::Throw` in a function with no throws slot; \
             `throw_or_escape` cannot produce this"
        ),
        Term::Jump(t) => emit_jump(out, types, f, cur, t),
        Term::Branch { cond, then, els } => {
            let _ = writeln!(out, "if ({}) {{", var(*cond));
            emit_jump(out, types, f, cur, then);
            out.push_str("} else {\n");
            emit_jump(out, types, f, cur, els);
            out.push_str("}\n");
        }
        Term::Switch { on, arms, default } => {
            let _ = writeln!(out, "switch ({}) {{", var(*on));
            for (k, t) in arms {
                let _ = writeln!(out, "case {}: {{", switch_key(k));
                emit_jump(out, types, f, cur, t);
                out.push_str("}\n");
            }
            out.push_str("default: {\n");
            emit_jump(out, types, f, cur, default);
            out.push_str("}\n    }\n");
        }
        Term::Unreachable => out.push_str("neon_unreachable();\n"),
    }
}

/// A jump: assign the target block's parameters from the arguments (coercing each into the
/// parameter's repr), then `goto`.
///
/// A BACKWARD jump (target at or before the current block) is a loop back-edge, and in a
/// fiber program it carries a preemption safepoint: the cheap flag check that lets the
/// timer stop a hot loop from starving every other fiber (docs/design/fibers.md). Only in a
/// fiber program — one that touches no fiber native emits exactly what it always did, so
/// non-fiber programs pay nothing and stay byte-identical. Block ids are emitted in order,
/// which is what makes "at or before" the back-edge test.
fn emit_jump(out: &mut String, types: &TypeTable, f: &Func, cur: u32, t: &Target) {
    let params = &f.block(t.to).params;
    for (&p, &a) in params.iter().zip(&t.args) {
        if p != a {
            let _ = writeln!(
                out,
                "{} = {};",
                var(p),
                coerce(types, f, a, f.value_repr(p))
            );
        }
    }
    if types.fiber_program && t.to.0 <= cur {
        out.push_str("neon_fiber_safepoint();\n");
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
                // Passing the argument uncoerced is precisely the failure `coerce` exists
                // to prevent: a narrow value written into a wider parameter slot, read back
                // past its own end. A miss means either a call to a function not in the
                // program or more arguments than parameters, neither of which the checker
                // lets through — so it refuses rather than emitting the unchecked copy.
                .map(|(i, &a)| match params.and_then(|p| p.get(i)) {
                    Some(t) => coerce(types, f, a, t),
                    None => panic!(
                        "internal error: codegen reached argument {i} of a call to `{func}` \
                         with no parameter repr to coerce it into"
                    ),
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
            // CORRECT DEFAULT: a closure call whose result is unused is a call for effect,
            // and `c_ret_type` turns `Unit` into `void` — which is what the callee's own
            // signature says for a unit-returning function, so the cast still matches.
            let ret = result
                .map(|v| f.value_repr(v))
                .cloned()
                .unwrap_or(Repr::Unit);
            let params: Vec<String> = std::iter::once("neon_header*".to_string())
                .chain(args.iter().map(|&v| types.c_type(f.value_repr(v))))
                .collect();
            // Call through the stored function pointer, cast to the concrete signature.
            let fnty = format!("{} (*)({})", c_ret_type(types, &ret), params.join(", "));
            let mut a = vec![format!("{}.env", var(*callee))];
            a.extend(args.iter().map(|&v| var(v)));
            format!("(({fnty}){}.fn)({})", var(*callee), a.join(", "))
        }
        Op::MakeRecord { fields, .. } => {
            // CORRECT DEFAULT, and inert. A `MakeRecord` whose result is dropped has no
            // struct to name; `Unit` makes `c_type` spell `neon_unit`, and the initialiser
            // list built below would then be `(neon_unit){.f_x = ..}` — rejected by `cc`,
            // loudly, at the one place the mistake is visible. It is not a value that can
            // be read back wrong, which is the property that matters here.
            let repr = result
                .map(|v| f.value_repr(v))
                .cloned()
                .unwrap_or(Repr::Unit);
            // A recursive record lives on the heap, so what is built is its pointee shape;
            // the fields are read off that, not off the pointer.
            let shape = types.boxed_shape(&repr).map(|(_, s)| s.clone());
            let ty = match &shape {
                Some(s) => types.c_type(s),
                None => types.c_type(&repr),
            };
            // CORRECT DEFAULT: `boxed_shape` is `None` for everything that is not a boxed
            // record, which is the ordinary case — the record is its own layout.
            let laid_out = shape.clone().unwrap_or_else(|| repr.clone());
            // Each field value is coerced into the field's declared repr (so a concrete
            // value flowing into a union or nullable field is injected).
            let field_repr = |n: &str| match &laid_out {
                Repr::Record { fields, .. } => fields
                    .iter()
                    .find(|(fname, _)| fname == n)
                    .map(|(_, r)| r.clone()),
                _ => None,
            };
            let inits: Vec<String> = fields
                .iter()
                .map(|(n, v)| {
                    // Same reasoning as the call above: an uncoerced field initialiser is a
                    // value stored at its own narrow width in the field's wider slot. The
                    // fields being initialised come from the record being built, so a name
                    // missing from that record's own layout is a codegen bug, not input.
                    let val = match field_repr(n) {
                        Some(t) => coerce(types, f, *v, &t),
                        None => panic!(
                            "internal error: codegen reached record field `{n}`, which the \
                             layout being built does not declare: {laid_out:?}"
                        ),
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
                        let i = union_field_index(types, variants, field);
                        format!("{inner}.u._{i}.{}", field_name(field))
                    }
                    _ => format!("{inner}.{}", field_name(field)),
                };
            }
            match brepr {
                // Accessing a field of a union value: project to the variant that has it.
                Repr::Union(variants) => {
                    let i = union_field_index(types, variants, field);
                    format!("{}.u._{i}.{}", var(*base), field_name(field))
                }
                _ => format!("{}.{}", var(*base), field_name(field)),
            }
        }
        Op::MakeTuple(elems) => {
            // As in `MakeRecord`: a discarded tuple has no struct to name, and `neon_unit`
            // with element initialisers does not compile rather than miscompiling.
            let repr = result
                .map(|v| f.value_repr(v))
                .cloned()
                .unwrap_or(Repr::Unit);
            let ty = types.c_type(&repr);
            let inits: Vec<String> = elems
                .iter()
                .enumerate()
                .map(|(i, v)| format!("._{i} = {}", var(*v)))
                .collect();
            format!("({ty}){{{}}}", inits.join(", "))
        }
        Op::Elem { base, index } => format!("{}._{index}", var(*base)),
        Op::Retain(v) => rc_value(types, f, "neon_retain", *v),
        Op::Release(v) => rc_value(types, f, "neon_release", *v),
        Op::IsErr(v) => format!("({}.tag == 1)", var(*v)),
        Op::UnwrapOk(v) => format!("{}.u._0", var(*v)),
        Op::UnwrapErr(v) => format!("{}.u._1", var(*v)),
        Op::IsNull(v) => is_null(f, *v),
        Op::IsVariant {
            value,
            variant,
            tested,
        } => match f.value_repr(*value) {
            // A type the union does not contain is one the value cannot be, so the answer
            // is `false` — not `tag == 0`. That fallback made `x is C` on an `A | B` come
            // back *true* for every `A`, because variant 0 is `A` and `tag == 0` is the
            // test for holding it. The checker allows the test (it is a legitimate question
            // with a known answer), so this is reachable from ordinary source.
            Repr::Union(variants) => {
                match variants
                    .iter()
                    .position(|v| names_variant(types, v, variant, tested.as_ref()))
                {
                    Some(i) => format!("({}.tag == {i})", var(*value)),
                    None => "false".into(),
                }
            }
            // An erased value carries its concrete type as a tag in its box. The tag is a
            // function of the *repr*, and the only way this comparison can be sound is for
            // it to call the very function that stamped the box — `type_tag`, via the type
            // the checker resolved the test to. Hashing `variant`, the head name the source
            // wrote, is what this used to do: `List[i64] is List[str]` was true, and the
            // `as` that a person writes after such a guard then read an i64 as a `neon_str`.
            //
            // Ices rather than falling back to the name. Without a resolved type there is
            // no honest answer here, and the dishonest one is a segfault in user code.
            Repr::Any => match tested {
                Some(t) => erased_tag_test(types, &var(*value), t),
                None => panic!(
                    "internal error: codegen reached `is {variant}` on an erased value with \
                     no resolved type; the checker must record one (see \
                     TypecheckResult::tested)"
                ),
            },
            // A nullable is a two-variant union whose discriminant is the pointer itself,
            // so `x is T` is a *runtime* null test, not a static answer. Falling through to
            // the concrete arm below folded it to `true`, because `type_tag_name` names a
            // `Nullable(T)` after its payload: a `while cur is str` on a `str | null` then
            // looped forever with `cur` null inside the body.
            Repr::Nullable(inner) => {
                if variant == "null" {
                    is_null(f, *value)
                } else if names_variant(types, inner, variant, tested.as_ref()) {
                    format!("(!{})", is_null(f, *value))
                } else {
                    "false".into()
                }
            }
            // A concrete RECORD tested against a record shape is a runtime question
            // wherever a field is declared wider than the shape asks for. `names_variant`
            // compares the two reprs' tag names, which is a *static* answer — and for
            //
            //     type Wide = { v: i64 | str }    type Narrow = { v: str }
            //
            // those names differ, so `w is Narrow` was false for every `w`, including the
            // ones holding a str. Silently, and in the arm-selecting direction: the value
            // took the other branch.
            //
            // Opacity does not constrain this. `s is { code: i64 }` on an opaque record is
            // rejected by the CHECKER (`records/opaque_no_structural_test.neon` is a
            // compile-fail), so a structural test against an opaque shape never reaches
            // codegen, and answering the reachable ones honestly cannot launder anything.
            rec @ Repr::Record { .. } => match tested.as_ref() {
                Some(t) => record_shape_test(types, &var(*value), rec, t),
                None => names_variant(types, rec, variant, None).to_string(),
            },
            // A value of one concrete type is that variant only if it *is* that type —
            // `r is Green` where `r` is a `Red` is false, not vacuously true.
            other => names_variant(types, other, variant, tested.as_ref()).to_string(),
        },
        Op::Cast(v) => {
            // CORRECT DEFAULT: a cast whose result is discarded has no target to move the
            // value into. `Any` boxes it and the statement is then thrown away — the value
            // never reaches a slot, so no reader can misread it.
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
            return format!(
                "({}){{ .{} = {expr} }}",
                types.c_type(target),
                field_name("#inner")
            );
        }
    }
    // Recovering a concrete value from `any`: check the box's tag against the target's,
    // then read the payload back out. The tag is the same `type_tag` the boxing site
    // stamped, so honest recovery (`(li: any) as List[i64]` after erasing one) passes and
    // a mismatch traps. An unchecked read here was a soundness hole for every type — the
    // payload came back reinterpreted at whatever the target claimed — and the working
    // forge of an opaque record (`let a: any = { code: 99 }; a as vault::Secret`), which
    // no static rule can reject without also rejecting the recovery idiom, because `any`
    // can legitimately hold either.
    //
    // Tags are canonical (see `coerce_expr`): a box always carries a concrete member's
    // tag, never a union's. So a union or nullable TARGET is a membership question, not
    // one compare: test each member's tag and inject that member's payload. The last
    // member reads through `neon_box_expect`, so a box holding none of them traps there.
    if matches!(src, Repr::Any) && !matches!(target, Repr::Any) {
        return unbox_expr(types, expr, target);
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
        // A union with no `null` arm can never be null, and saying so is the whole point:
        // falling back to index 0 asked `tag == 0`, which is *true* whenever the value
        // holds its first variant. `x == null` on an `i64 | str` would have answered yes
        // for every `i64`.
        Repr::Union(variants) => match variants.iter().position(|r| matches!(r, Repr::Null)) {
            Some(i) => format!("({}.tag == {i})", var(v)),
            None => "false".into(),
        },
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

/// Whether the box at `expr` holds a value of type `t`. Tags are canonical — always a
/// concrete member's, never a union's (see `coerce_expr`) — so a union or nullable `t`
/// is a disjunction over its members' tags rather than one compare against a tag no box
/// ever carries.
fn erased_tag_test(types: &TypeTable, expr: &str, t: &Repr) -> String {
    match types.resolve(t) {
        Repr::Union(variants) => {
            let arms: Vec<String> = variants
                .iter()
                .map(|v| format!("neon_box_tag({expr}) == {}ULL", types.type_tag(v)))
                .collect();
            format!("({})", arms.join(" || "))
        }
        Repr::Nullable(inner) => format!(
            "(neon_box_tag({expr}) == {}ULL || neon_box_tag({expr}) == {}ULL)",
            types.type_tag(&Repr::Null),
            types.type_tag(inner),
        ),
        other => format!("(neon_box_tag({expr}) == {}ULL)", types.type_tag(other)),
    }
}

/// Recover a value of `target` from the box at `expr`: the tag-checked read behind every
/// cast and narrowed flow out of `any`. Tags are canonical (see `coerce_expr`), so a
/// union or nullable target is a membership chain over its members' tags — the last
/// member reads through `neon_box_expect`, so a box holding none of them traps there —
/// and a concrete target is one checked read.
fn unbox_expr(types: &TypeTable, expr: &str, target: &Repr) -> String {
    if let Repr::Union(variants) = target {
        let ty = types.c_type(target);
        let last = variants.len() - 1;
        let mut out = format!(
            "({ty}){{ .tag = {last}, .u._{last} = (*({}*)neon_box_expect({expr}, {}ULL)) }}",
            types.c_type(&variants[last]),
            types.type_tag(&variants[last]),
        );
        for (i, vr) in variants.iter().enumerate().take(last).rev() {
            out = format!(
                "(neon_box_tag({expr}) == {}ULL ? ({ty}){{ .tag = {i}, .u._{i} = \
                 (*({}*)neon_box_payload({expr})) }} : {out})",
                types.type_tag(vr),
                types.c_type(vr),
            );
        }
        return out;
    }
    if let Repr::Nullable(inner) = target {
        // A boxed null recovers as the nullable's null value (its zero); anything else
        // must be the payload type or trap.
        return format!(
            "(neon_box_tag({expr}) == {}ULL ? ({}){{0}} : (*({}*)neon_box_expect({expr}, {}ULL)))",
            types.type_tag(&Repr::Null),
            types.c_type(target),
            types.c_type(target),
            types.type_tag(inner),
        );
    }
    format!(
        "(*({}*)neon_box_expect({expr}, {}ULL))",
        types.c_type(target),
        types.type_tag(target),
    )
}

/// One boxing: the value at C expression `expr`, of repr `src`, into a fresh `any` box
/// stamped with `src`'s witness and tag. The one-element array compound literal gives the
/// payload an address without needing a named temp.
fn box_expr(types: &TypeTable, expr: &str, src: &Repr) -> String {
    format!(
        "neon_box_new(({}[]){{{expr}}}, {}, {}ULL)",
        types.c_type(src),
        types.witness_ref(src),
        types.type_tag(src),
    )
}

/// The null test for a nullable's C value at expression `expr` — the string-valued twin
/// of `is_null`'s `Nullable` arms, for sites that hold an expression rather than a
/// `Value`.
fn nullable_is_null_expr(expr: &str, inner: &Repr) -> String {
    match inner {
        Repr::Str => format!("({expr}.data == NULL)"),
        Repr::Closure { .. } => format!("({expr}.fn == NULL)"),
        _ => format!("({expr} == NULL)"),
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
        // CORRECT DEFAULT: a union of nothing but `null` has no non-null variant to
        // collapse to, and the union itself is the honest answer — `Null` is a `neon_unit`
        // with one inhabitant, so every caller of this treats it as an aggregate and
        // compares it with `eq_expr`, which answers without reading any bytes.
        Repr::Union(variants) => variants
            .iter()
            .find(|v| !matches!(v, Repr::Null))
            .unwrap_or(r),
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
        eq_expr(
            types,
            other_repr,
            &format!("{}.u._{i}", var(u)),
            &prim_operand(f, other)
        ),
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
            // CORRECT DEFAULT: reached only when every variant is `Null`, in which case
            // index 0 *is* the null variant — the right slot, not a guess at one.
            let i = variants
                .iter()
                .position(|r| !matches!(r, Repr::Null))
                .unwrap_or(0);
            format!("{}.u._{i}", var(v))
        }
        _ => var(v),
    }
}

/// The name an `is Name` test asks for. A union's variants are not only records: a union
/// of primitives (`i64 | str | bool`) is tested by type name, so this uses the same naming
/// as the boxed type tag rather than recognising records alone.
/// Whether `r` is the variant the source named in an `is` test.
///
/// A recursive record is a `BoxedRec` carrying an atom id, not a `Record`, so its own name
/// is only reachable through the type table. Comparing without this resolution answered
/// `record Node { next: Node | null }`'s `x is Node` with the *key* of a boxed repr, which
/// never equals `Node`: every `is` against a recursive record was silently false, and the
/// union arm's positional search fell back to variant 0.
///
/// `tested` — the type the checker resolved the test to — is the answer whenever it is
/// available, and then the comparison is between two *tag names*: one derivation, applied
/// to both sides, the same one `type_tag` hashes into a box. This is what lets a union
/// distinguish `List[i64]` from `List[str]`, which a head-name comparison cannot, and what
/// keeps the union arm agreeing with the erased arm about what `is` means.
///
/// The `variant` fallback is for the tests the checker records no type for — a record
/// pattern under an error. It compares head names, which is what this did for everything
/// before, and is sound only because a union's arms are distinct *nominal* types.
/// Whether the record at `expr`, declared as `src`, currently holds a value of the record
/// shape `target` — as a C expression, because the answer generally is not static.
///
/// A record matches a narrower shape when every field the shape names is present and
/// currently holds a value of the shape's field type. A field declared at exactly the
/// shape's type contributes nothing; one declared as a union contributes a tag test; one
/// declared as a record recurses. That conjunction is the whole test.
///
/// Undecidable cases fall back to the static comparison rather than to `false`, so this only
/// ever answers where it can answer correctly:
///
/// - a `target` that is not a record at all (a tuple, an arrow) — not this function's shape
/// - a field whose declared repr is neither identical, a union containing the target's, nor
///   a record to recurse into — a nullable, say, where the honest test is a null check this
///   does not yet write
///
/// Both keep today's behaviour, which is wrong in the same direction as before but no more
/// so. Widening the decidable set is a matter of adding arms here.
fn record_shape_test(types: &TypeTable, expr: &str, src: &Repr, target: &Repr) -> String {
    let src = types.resolve(src);
    let target = types.resolve(target);
    let (
        Repr::Record { fields: sf, .. },
        Repr::Record {
            name: tn,
            fields: tf,
        },
    ) = (src, target)
    else {
        return names_variant(types, src, "", Some(target)).to_string();
    };
    // Only a STRUCTURAL target is a shape question. A named one is a nominal identity
    // question and stays with `names_variant`, which is the rule that keeps
    // `match s { is Point => .. }` from matching an unrelated record with the same fields —
    // and, for a unit record, from matching everything: with no fields to test, the
    // conjunction below is vacuously true, so `is Empty` answered yes for every record in
    // the program. `records/unit_record.neon` and `is_sum_type.neon` are that rule.
    if tn.is_some() {
        return names_variant(types, src, "", Some(target)).to_string();
    }
    let mut tests: Vec<String> = Vec::new();
    for (name, want) in tf {
        // A shape naming a field the value does not have is not that shape. This is a
        // static "no" rather than an undecidable case.
        let Some((_, have)) = sf.iter().find(|(n, _)| n == name) else {
            return "false".to_string();
        };
        let access = format!("{expr}.{}", field_name(name));
        let have = types.resolve(have);
        let want = types.resolve(want);
        if have == want {
            continue;
        }
        match have {
            // The field is a union and the shape wants one of its variants: compare the tag
            // the injection stamped. A shape wanting something the union cannot hold is a
            // static "no".
            Repr::Union(variants) => match variants.iter().position(|v| types.resolve(v) == want) {
                Some(i) => tests.push(format!("({access}.tag == {i})")),
                None => return "false".to_string(),
            },
            Repr::Record { .. } => tests.push(record_shape_test(types, &access, have, want)),
            _ => return names_variant(types, src, "", Some(target)).to_string(),
        }
    }
    if tests.is_empty() {
        return "true".to_string();
    }
    tests.join(" && ")
}

fn names_variant(types: &TypeTable, r: &Repr, variant: &str, tested: Option<&Repr>) -> bool {
    if let Some(t) = tested {
        return match (variant_tag(types, r), variant_tag(types, t)) {
            (Some(a), Some(b)) => a == b,
            // `never` and `any` name no single runtime type, and a `Var` has no tag at
            // all; none of them can be the variant a value holds.
            _ => false,
        };
    }
    let r = types.resolve(r);
    if let Some((_, pointee)) = types.boxed_shape(r) {
        return variant_name(types, pointee).as_deref() == Some(variant);
    }
    variant_name(types, r).as_deref() == Some(variant)
}

/// A repr's tag name for comparison purposes, with back-edges and boxes resolved so the
/// two sides of an `is` are spelled from the same shape. `None` where there is no single
/// type to name.
fn variant_tag(types: &TypeTable, r: &Repr) -> Option<String> {
    let r = types.resolve(r);
    let r = types.boxed_shape(r).map_or(r, |(_, pointee)| pointee);
    match r {
        Repr::Var(_) | Repr::Never | Repr::Any => None,
        other => Some(types.type_tag_name(other)),
    }
}

fn variant_name(types: &TypeTable, r: &Repr) -> Option<String> {
    match r {
        Repr::Record { name, .. } => name.clone(),
        Repr::Var(_) | Repr::Never | Repr::Any => None,
        other => Some(types.type_tag_name(other)),
    }
}

/// Whether a variant is a record carrying the named field.
///
/// Resolved through a back-edge and a box first, for the reason `names_variant` is: a
/// self-referencing record reaches here as a `BoxedRec` atom id and a `mu` type as a
/// `Recursive`, neither of which is a `Repr::Record`. Asking the bare repr answered "no"
/// for every variant of `A | B` when `A` and `B` are recursive, and the caller's positional
/// search then fell back to variant 0 -- reading the field at another variant's offset.
fn record_has_field(types: &TypeTable, r: &Repr, field: &str) -> bool {
    let r = types.resolve(r);
    let r = types.boxed_shape(r).map_or(r, |(_, pointee)| pointee);
    matches!(r, Repr::Record { fields, .. } if fields.iter().any(|(n, _)| n == field))
}

/// The index of the union variant carrying `field`.
///
/// Ices rather than defaulting. The `unwrap_or(0)` this replaces produced a *type-correct*
/// C field access into the wrong arm of the payload union -- a read at whatever offset
/// variant 0 puts that field name at, or at no valid offset at all. That is the exact
/// shape of the `Op::IsVariant` bug: an answer where there should have been a refusal.
fn union_field_index(types: &TypeTable, variants: &[Repr], field: &str) -> usize {
    match variants
        .iter()
        .position(|v| record_has_field(types, v, field))
    {
        Some(i) => i,
        None => panic!(
            "internal error: codegen reached a field access for `{field}`, which no variant \
             of the union carries: {variants:?}"
        ),
    }
}

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
    format!(
        "({}[]){{{}}}",
        types.c_type(target),
        coerce(types, f, v, target)
    )
}

/// Coerce a value into a target repr at a flow site: the `Value` form of `coerce_expr`.
fn coerce(types: &TypeTable, f: &Func, v: Value, target: &Repr) -> String {
    coerce_expr(types, &var(v), f.value_repr(v), target)
}

/// Move a C expression from the repr it has to the repr its destination wants.
///
/// This is the widening direction only — injection into a union, boxing into `any`, width
/// and covariant subtyping on records and tuples. Narrowing is `cast_expr`, which tries
/// the projections first and then delegates here for everything that is not a projection.
///
/// It must be applied at *every* flow site, because the IR is typed more precisely than
/// the slots values land in: the IR knows a literal is an `i64`, while the field it is
/// being stored into is an `i64 | str` whose C struct is wider. Assigning without coercing
/// compiles — C is happy to convert — and then the reader takes the tag from whatever
/// happened to be in the adjacent bytes.
///
/// The `{0}` at the end is not a fallback for "shapes we did not handle". It is the arm of
/// a branch the checker already proved dead, which still has to compile; if a live value
/// reaches it, the bug is upstream, in whatever produced a repr pair that cannot convert.
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
    //
    // The tag must be CANONICAL — a function of the value's concrete type, not of the
    // static type at this erasure site. A union-typed value is therefore switched on its
    // discriminant and the *member* is boxed with the member's tag (a null member boxes
    // as null); a nullable is the two-variant case of the same rule. Boxing the union
    // whole stamped `type_tag(union)`, so the same logical value carried a different tag
    // depending on which site erased it, and every `is`/cast on the erased value then
    // answered from the wrong tag — erasing a narrowed member at a union-typed join made
    // `is A` false on a value that was exactly an `A`. Every checked cast rests on this
    // invariant: a tag proves what was genuinely constructed.
    if matches!(target, Repr::Any) {
        if let Repr::Union(from) = src {
            let last = from.len() - 1;
            let mut out = box_expr(types, &format!("{expr}.u._{last}"), &from[last]);
            for (i, sv) in from.iter().enumerate().take(last).rev() {
                out = format!(
                    "({expr}.tag == {i} ? {} : {out})",
                    box_expr(types, &format!("{expr}.u._{i}"), sv)
                );
            }
            return out;
        }
        if let Repr::Nullable(inner) = src {
            // The non-null branch boxes the nullable's own C value: it shares its
            // representation with the payload, and `type_tag` already names a nullable
            // after its payload, so the tag is the member's.
            return format!(
                "({} ? {} : {})",
                nullable_is_null_expr(expr, inner),
                box_expr(types, "neon_null()", &Repr::Null),
                box_expr(types, expr, src),
            );
        }
        return box_expr(types, expr, src);
    }
    if let Repr::Union(variants) = target {
        if let Some(i) = variants.iter().position(|vr| vr == src) {
            return format!(
                "({}){{ .tag = {i}, .u._{i} = {expr} }}",
                types.c_type(target)
            );
        }
        // Widening one union into a larger one. The two carry independent tag numberings,
        // so this is a *runtime* remap of the discriminant, not a reinterpretation: in
        // `Running | Done` -> `Running | Paused | Done`, tag 1 means Done on the left and
        // Paused on the right. Without this the coercion fell through to the zeroed
        // literal at the end, which is how `fn f() -> A | B | C` whose branches only ever
        // produce `A` or `C` returned a zeroed `A` — a wrong value, no diagnostic.
        //
        // `expr` repeats across the arms; every caller passes an SSA variable or a pure
        // field/element projection of one, so re-evaluation is free of effects.
        if let Repr::Union(from) = src {
            let ty = types.c_type(target);
            let mut out = format!("({ty}){{0}}");
            for (i, sv) in from.iter().enumerate().rev() {
                // CORRECT DEFAULT: a source variant absent from the target is one the
                // widening cannot be carrying, so it gets no arm and falls through to the
                // zeroed literal — the dead branch documented at the end of this function.
                let Some(j) = variants.iter().position(|tv| tv == sv) else {
                    continue;
                };
                out = format!(
                    "({expr}.tag == {i} ? ({ty}){{ .tag = {j}, .u._{j} = {expr}.u._{i} }} : {out})"
                );
            }
            return out;
        }
    }
    // The NARROWING direction at a flow site: the checker proved the value holds this
    // member — a match arm's rebound scrutinee, an `is`-guard's refined subject — and a
    // narrowed value flows at its narrowed type without a written cast. Projection is
    // what that means; before this arm existed, a bare use of a narrowed union fell
    // through to the zeroed literal below and `match v { is A => takes_a(v) }` passed a
    // zeroed `A`, silently.
    if let Repr::Union(from) = src {
        if let Some(i) = from.iter().position(|vr| vr == target) {
            return format!("{expr}.u._{i}");
        }
        // No exact member: switch on the discriminant and widen EACH variant into the
        // target. This is how a join typed at a union of shapes flows into one wider
        // shape — `if c { (true, 4) } else { (null, 5) }` is a `(bool, i64) |
        // (null, i64)` at the join, and an annotated `(J, i64)` slot needs the live
        // arm's tuple rebuilt element-wise, not the union's bytes reinterpreted.
        // Falling to the zeroed literal here is how the `i64` tail of that tuple came
        // back 0. A variant that genuinely cannot widen still zeroes — but
        // per variant, on an arm the checker proved dead.
        let last = from.len() - 1;
        let mut out = coerce_expr(types, &format!("{expr}.u._{last}"), &from[last], target);
        for (i, sv) in from.iter().enumerate().take(last).rev() {
            out = format!(
                "({expr}.tag == {i} ? {} : {out})",
                coerce_expr(types, &format!("{expr}.u._{i}"), sv, target)
            );
        }
        return out;
    }
    // Narrowed `any`: the checker refined an erased value to a concrete type; unbox it
    // through the tag check. Sound because tags are canonical — and if the refinement is
    // ever wrong, this traps rather than reinterpreting the payload.
    if matches!(src, Repr::Any) {
        return unbox_expr(types, expr, target);
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
                // CORRECT DEFAULT only in the sense that it cannot mislead: a source tuple
                // shorter than its target has no element `i` to widen, and coercing `tr` to
                // itself emits `{expr}._{i}` — a member the source struct does not have, so
                // `cc` rejects it. Width subtyping on tuples only ever drops from the right.
                let sr = se.get(i).unwrap_or(tr);
                format!(
                    "._{i} = {}",
                    coerce_expr(types, &format!("{expr}._{i}"), sr, tr)
                )
            })
            .collect();
        return format!("({}){{{}}}", types.c_type(target), inits.join(", "));
    }
    // Covariance on a CONTAINER is not free the way it is on a record. `List[i64]` and
    // `List[i64 | str]` are both `neon_list*`, so the arm below would call them
    // representationally identical and pass the pointer through -- and the elements are
    // stored inline, `int64_t` slots against union structs, so the reader takes each one at
    // the wrong width. That was a silent miscompile for `plain(xs)` with `xs: List[i64]`
    // against a `List[i64 | str]` parameter.
    //
    // Widening a list therefore means building a new one, element by element. The shim is
    // generated per element-repr pair and requested here, mid-emission, because a coercion
    // is not an `Op` and there is nothing to scan for ahead of time.
    if let (Repr::List(se), Repr::List(te)) = (src, target) {
        let (se, te) = (types.resolve(se).clone(), types.resolve(te).clone());
        if se != te {
            let name = list_conv_name(types, &se, &te);
            types.need_list_conv(&name, &se, &te);
            return format!("{name}({expr})");
        }
    }
    // A `Map` is the same story as a `List` above: same `neon_map*`, different slot widths,
    // so widening either half means rebuilding the table entry by entry.
    if let (Repr::Map(sk, sv), Repr::Map(tk, tv)) = (src, target) {
        let same = types.resolve(sk) == types.resolve(tk) && types.resolve(sv) == types.resolve(tv);
        if !same {
            let name = map_conv_name(types, src, target);
            types.need_map_conv(&name, src, target);
            return format!("{name}({expr})");
        }
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

/// `seen` is the chain of back-edges currently being resolved.
///
/// A recursive type normally terminates before the guard matters: either through a pointer
/// (a `List`, a closure) or because the cycle was boxed, and the `is_boxed` check above
/// counts the whole thing with a single `neon_retain` on its header without looking inside.
/// Re-entering a back-edge means neither happened — a cycle that closes entirely by value
/// with nothing to stop the walk. Such a type has no finite layout and cannot be counted in
/// place at all, so stopping is the only option that does not recurse until the stack dies;
/// `cc` then rejects the infinite struct with a diagnostic naming the type.
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
        // Through `neon_str_retain`/`neon_str_release` rather than reaching for `.owner`
        // ourselves. A string's representation is the runtime's business, and this is the
        // only place the backend would otherwise know it: routing through the accessor is
        // what lets a small-string optimisation change the layout without regenerating or
        // revisiting a line of emitted C. `func` is `neon_retain`/`neon_release`, so the
        // `neon_` prefix is replaced rather than appended.
        Repr::Str => out.push(format!(
            "neon_str_{}({expr})",
            func.trim_start_matches("neon_")
        )),
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
                    let _ = write!(chain, "{expr}.tag == {i} ? ((void)({})) : ", sub.join(", "));
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
                rc_parts_rec(
                    types,
                    func,
                    fr,
                    &format!("{expr}.{}", field_name(n)),
                    out,
                    seen,
                );
            }
        }
        Repr::Tuple(elems) => {
            for (i, e) in elems.iter().enumerate() {
                rc_parts_rec(types, func, e, &format!("{expr}._{i}"), out, seen);
            }
        }
        // Nothing counted, spelled out rather than left to a catch-all. A missing arm here
        // does not fail to compile — it emits a value that is simply never retained or
        // released, which is a leak or a use-after-free depending on the direction, and
        // shows up nowhere until a sanitizer is pointed at it. Listing them makes adding a
        // `Repr` variant a compile error at this site.
        //
        // Scalars own no memory. `Never` is uninhabited. `BoxedRec` and `Recursive` are
        // handled above, before this match.
        Repr::I64
        | Repr::F64
        | Repr::Bool
        | Repr::Tag
        | Repr::Unit
        | Repr::Null
        | Repr::Never
        | Repr::BoxedRec(_)
        | Repr::Recursive(_) => {}
        Repr::Var(_) => ice(repr, "refcounting a type variable"),
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

/// A primitive operation.
///
/// The bulk of the function is the equality/comparison prologue, not the operator table,
/// because "compare these two values" in Neon is structural on every type while C's `==`
/// works on scalars alone. Three cases have to be caught before the operands are projected
/// to scalars, and each corresponds to a bug that shipped: a union against a bare variant
/// (must test the tag, or a `null` payload of zero matches the literal `0`), a nullable
/// against a literal `null` (the two sides have different reprs, so it is a null *test*,
/// not a compare), and two whole unions (projecting both to their first variant compared
/// an `i64` against a `bool` and made `1 == true` true).
///
/// Below that, integer arithmetic goes through `neon_i64_*` rather than C operators: signed
/// overflow and division by zero are undefined behaviour in C and defined traps in Neon.
/// Float arithmetic is plain, being IEEE in both languages.
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
    let is_float = args
        .first()
        .is_some_and(|&v| matches!(scalar(v), Repr::F64));
    let is_str = args
        .first()
        .is_some_and(|&v| matches!(scalar(v), Repr::Str));
    // CORRECT DEFAULT: `b` is absent for a unary primop (`Neg`, `Not`, `Bnot`), and every
    // arm that reads it is binary. An empty `a` would mean a nullary primop, of which there
    // are none — and it would produce syntactically invalid C, not a wrong answer.
    let a = args
        .first()
        .map(|&v| prim_operand(f, v))
        .unwrap_or_default();
    let b = args.get(1).map(|&v| prim_operand(f, v)).unwrap_or_default();

    // Comparison is structural on every type (docs/decisions.md). C has no `==` for a
    // struct, so an aggregate expands fieldwise, and `str` ordering needs the runtime's
    // bytewise compare. Scalars fall through to the plain operators below.
    if matches!(
        op,
        PrimOp::Eq | PrimOp::Ne | PrimOp::Lt | PrimOp::Le | PrimOp::Gt | PrimOp::Ge
    ) {
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
        // `fmod` is the truncating remainder — result takes the dividend's sign — which
        // matches `neon_i64_rem` and `/`'s truncation toward zero. This arm was MISSING:
        // the i64 line below took floats too, and C converted each `double` argument to
        // `int64_t` without a word, so `5.5 % 2.5` compiled, ran, and printed `1`.
        PrimOp::Rem if is_float => format!("fmod({a}, {b})"),
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
        // `bsr` is ARITHMETIC — the sign extends — and the language guarantees it, so
        // it is spelled in defined operations rather than left to a bare `>>` on
        // int64_t, which C11 §6.5.7p5 makes implementation-defined for a negative
        // left operand (gcc and clang both happen to choose arithmetic; "happen to"
        // is what this line removes). For a negative value the identity
        // `a >> n == ~(~a >> n)` runs the shift on the complement — non-negative, so
        // the unsigned logical shift agrees — and complements back; both `~` are on
        // uint64_t, so every step is defined.
        PrimOp::Bsr => format!(
            "((int64_t)({a} < 0 ? ~(~(uint64_t){a} >> ({b} & 63)) : (uint64_t){a} >> ({b} & 63)))"
        ),
        PrimOp::Bnot => format!("(~{a})"),
    }
}

// ---- C names ----

/// `nl_` + an injective escape of the IR name into a valid C identifier. Runtime symbols
/// (`neon_*`) are never mangled.
///
/// INJECTIVITY OBLIGATION: this is an identity — it is the C symbol, so a collision here
/// is two Neon functions becoming one linker name, which is the `repr_key` bug arriving
/// at the very last step. It is injective over ASCII, and only over ASCII.
///
/// The argument: `_` is the sole escape lead and is itself always escaped (`_u`), so an
/// underscore in the output can only ever begin an escape, and decoding is unambiguous —
/// `_u`, `_S`, or `_x` plus EXACTLY TWO hex digits. That last clause is where the ASCII
/// bound comes from: `{:02x}` is a minimum width, not a fixed one, so a character above
/// `0xff` writes more than two digits and the escape stops being self-delimiting.
/// `mangle("→")` and `mangle("!92")` would then both be `nl__x2192`.
///
/// The bound holds because IR names are built from identifiers, `$` separators and
/// `repr_key` output, and the lexer admits only `[A-Za-z_][A-Za-z0-9_]*`
/// (`lexer/mod.rs::is_ident_start`/`is_ident_continue`) — every one of which is ASCII.
/// The assertion below is what enforces it rather than merely claiming it, so admitting
/// non-ASCII identifiers fails loudly here instead of silently merging two symbols.
fn mangle(name: &str) -> String {
    let mut out = String::from("nl_");
    for c in name.chars() {
        match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' => out.push(c),
            '$' => out.push_str("_S"),
            '_' => out.push_str("_u"),
            other => {
                debug_assert!(
                    other.is_ascii(),
                    "mangle got a non-ASCII character {other:?} in {name:?}; the \
                     `_x{{:02x}}` escape is only self-delimiting below 0x100, so this \
                     is no longer injective — `→` and `!92` both mangle to `nl__x2192`. \
                     Widen the escape to a delimited form before admitting these."
                );
                let _ = write!(out, "_x{:02x}", other as u32);
            }
        }
    }
    out
}

/// An SSA value's C local. The leading underscore keeps these out of the way of every
/// other name the emitter invents: mangled functions are `nl_`-prefixed, struct and
/// witness names carry their own prefixes, and record fields are `f_`-prefixed, so a
/// user-chosen name can never collide with a value slot.
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

/// A `switch` case label. Every key has to be an integer constant expression, which is why
/// atoms switch on their FNV-1a hash rather than on a name.
///
/// `Nominal` has no integer key. It used to emit `0` for every variant, which is not a
/// hole so much as a wrong answer: several `case 0:` labels in one `switch` do not even
/// compile, and a single nominal case silently switched on the constant 0. Nothing in
/// `ir/lower.rs` constructs a `SwitchKey::Nominal` today -- `ssa/print.rs` is its only
/// other mention -- so this refuses rather than guessing, and whoever does start lowering
/// nominal dispatch gets a panic naming the variant instead of a miscompile. Giving union
/// variants dense tags is the fix.
fn switch_key(k: &SwitchKey) -> String {
    match k {
        SwitchKey::Int(n) => c_i64(*n),
        SwitchKey::Bool(b) => (*b as i64).to_string(),
        SwitchKey::Atom(a) => atom_hash(a),
        SwitchKey::Nominal(n) => panic!(
            "internal error: codegen reached a nominal switch key `{n}` — union variants \
             have no dense integer tags to switch on yet"
        ),
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
            // OCTAL, not hex, and the difference is a miscompile. A C hex escape has no
            // length limit — it eats every hex digit that follows — so `"\x01" "a"`
            // written as `\x01a` is ONE character, `0x1a`, and the literal is a byte
            // shorter than the length handed to `neon_str_lit` beside it. `"\x01a"`
            // printed `1a 00`. An octal escape is capped at three digits, so `\001`
            // ends where it is written whatever follows it. This file already reasons
            // about maximal munch for `mangle`; the string literals had the same hazard
            // and not the same care.
            _ => {
                let _ = write!(out, "\\{b:03o}");
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

/// A short name for an op, used only in the `unreachable!` that fires when `op_rhs` has no
/// case for it. The named arms are all ops `op_rhs` or `emit_inst` does handle, so in
/// practice the panic reports the `"op"` catch-all; naming the op that actually escaped
/// means adding an arm here.
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
