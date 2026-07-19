//! Effect analysis, for the optimiser only. Pessimistic: a function is **effectful**
//! unless it can be cheaply proven pure. Being wrong in the safe direction only costs a
//! missed optimisation; the reverse would miscompile. See `docs/design/ir.md`.
//!
//! Because the language is immutable there is no read/write-of-mutable-memory category —
//! the whole lattice is two states, `pure` vs `effectful`. A function is pure iff every
//! instruction is pure and every callee is pure; the callee condition is a monotonic
//! fixpoint over the call graph.

use super::ssa::{Op, Program};
use std::collections::{HashMap, HashSet};

/// Whether a native symbol has an observable effect: I/O, or a panic/abort. Everything
/// else the runtime exposes (arithmetic, string and collection queries) is a pure
/// function of its arguments.
pub fn native_is_effectful(symbol: &str, pure_natives: &HashSet<String>) -> bool {
    !pure_natives.contains(symbol)
}

/// The set of functions proven pure, by name. A name absent from the set is effectful.
pub fn analyze(program: &Program) -> HashMap<String, bool> {
    // Start optimistic (every function pure), then knock out any that do something
    // effectful or reach one that does, to a fixpoint. Monotone, so it converges.
    let mut pure: HashMap<String, bool> = program.funcs.iter().map(|f| (f.name.clone(), true)).collect();

    loop {
        let mut changed = false;
        for f in &program.funcs {
            if !pure.get(&f.name).copied().unwrap_or(false) {
                continue; // already effectful
            }
            let effectful = f.blocks.iter().any(|b| {
                b.insts.iter().any(|inst| op_is_effectful(&inst.op, &pure, &program.pure_natives))
            });
            if effectful {
                pure.insert(f.name.clone(), false);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    pure
}

/// Whether an op has an effect that must be preserved -- so DCE may not drop it even if
/// its result is unused, and CSE may not share it. `pure` maps each function to whether
/// it is pure.
pub fn op_is_effectful(
    op: &Op,
    pure: &HashMap<String, bool>,
    pure_natives: &HashSet<String>,
) -> bool {
    match op {
        // Talks to the world, or reaches something that might. A native is opaque, so
        // this is what it *declared*: no `@pure`, no elimination.
        Op::Native { symbol, .. } => native_is_effectful(symbol, pure_natives),
        // A direct call is effectful iff its callee is; an unknown callee (not in the
        // program -- e.g. a not-yet-lowered instance) is assumed effectful.
        Op::Call { func, .. } => !pure.get(func).copied().unwrap_or(false),
        // An indirect call cannot be seen through: pessimistically effectful.
        Op::CallClosure { .. } => true,
        // Everything else is a pure function of its operands.
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::lower::lower_module;
    use crate::typecheck::{check::check_module, Env};
    use crate::{lexer, parser};

    fn analyze_src(src: &str) -> HashMap<String, bool> {
        let tokens = lexer::lex(src).expect("lexes");
        let (module, e) = parser::parse(&tokens, src.len());
        assert!(e.is_empty());
        let module = module.expect("parses");
        let mut env = Env::build(&module);
        assert!(env.errors().is_empty(), "{:?}", env.errors());
        let (result, errs) = check_module(&mut env, &module);
        assert!(errs.is_empty(), "{errs:?}");
        analyze(&lower_module(&env, &result, &module, &[]))
    }

    #[test]
    fn arithmetic_is_pure_io_is_not() {
        let e = analyze_src(
            "@native(\"neon_io_println\") fn println(s: str)
             fn double(x: i64) -> i64 { x + x }
             fn shout(s: str) { println(s); }
             fn calls_io(s: str) { shout(s); }",
        );
        assert_eq!(e.get("double"), Some(&true), "arithmetic is pure");
        assert_eq!(e.get("shout"), Some(&false), "calls io -> effectful");
        assert_eq!(e.get("calls_io"), Some(&false), "reaches io transitively");
    }

    #[test]
    fn a_pure_native_stays_pure() {
        let e = analyze_src(
            "@pure @native(\"neon_str_concat\") fn concat(a: str, b: str) -> str
             fn greet(n: str) -> str { concat(\"hi \", n) }",
        );
        assert_eq!(e.get("greet"), Some(&true), "string concat is declared pure");
    }

    /// The polarity that matters: an unannotated native is effectful, so a caller of one
    /// is effectful too and its calls survive DCE. Guessing purity from the symbol's
    /// spelling — the rule this replaced — deleted a resource construction and with it the
    /// cleanup that construction existed to schedule.
    #[test]
    fn an_unannotated_native_is_effectful() {
        let e = analyze_src(
            "@native(\"neon_str_concat\") fn concat(a: str, b: str) -> str
             fn greet(n: str) -> str { concat(\"hi \", n) }",
        );
        assert_eq!(e.get("greet"), Some(&false), "no `@pure` means effectful");
    }
}
