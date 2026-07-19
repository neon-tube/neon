//! Effect analysis, for the optimiser only. Pessimistic: a function is **effectful**
//! unless it can be cheaply proven pure. Being wrong in the safe direction only costs a
//! missed optimisation; the reverse would miscompile. See `docs/design/ir.md`.
//!
//! Because the language is immutable there is no read/write-of-mutable-memory category —
//! the whole lattice is two states, `pure` vs `effectful`. A function is pure iff every
//! instruction is pure and every callee is pure; the callee condition is a monotonic
//! fixpoint over the call graph.

use super::repr::Repr;
use super::ssa::{BlockId, Func, Op, PrimOp, Program, Term};
use std::collections::{HashMap, HashSet};

/// Whether a native symbol has an observable effect. A native's body is opaque to the
/// compiler, so this is not an analysis: it reports what the declaration *claimed*.
/// `pure_natives` is the set of symbols whose `@native` declaration also carried `@pure`,
/// and everything outside it is effectful.
///
/// The polarity is the load-bearing part. Silence means effectful, so forgetting `@pure`
/// costs an optimisation and nothing else, while a wrong `@pure` licenses DCE to delete a
/// call that mattered. The rule this replaced inferred purity from the symbol's spelling
/// and deleted a resource construction along with the cleanup that construction existed to
/// schedule; the test at the bottom of this file pins the direction.
pub fn native_is_effectful(symbol: &str, pure_natives: &HashSet<String>) -> bool {
    !pure_natives.contains(symbol)
}

/// Purity for every function in the program, keyed by name. A name that is *absent* is not
/// "unknown" but effectful: `op_is_effectful` reads a missing entry as false-purity, which
/// is how a call to something outside the lowered program — a native, an instance that has
/// not been monomorphised yet — stays un-eliminable.
///
/// The fixpoint starts optimistic and only ever removes purity, which is what lets
/// recursion terminate *and* be classified usefully: a self- or mutually-recursive
/// function is provisionally pure while its own body is examined, so a pure recursive
/// function stays pure instead of demoting itself on the first look at its own call.
/// Starting pessimistic would be sound but would mark every cycle effectful.
///
/// Keying by name means the mangled names monomorphisation produces must be distinct. Two
/// functions sharing one would share a verdict — the merge lands on effectful if either is,
/// so the result stays safe, but a pure instance would be needlessly pinned.
pub fn analyze(program: &Program) -> HashMap<String, bool> {
    // Seed: pure unless the function might not terminate, then knock out any that does
    // something effectful or reaches one that does, to a fixpoint. Monotone, so it
    // converges.
    let diverging = may_diverge(program);
    let mut pure: HashMap<String, bool> =
        program.funcs.iter().map(|f| (f.name.clone(), !diverging.contains(&f.name))).collect();

    loop {
        let mut changed = false;
        for f in &program.funcs {
            if !pure.get(&f.name).copied().unwrap_or(false) {
                continue; // already effectful
            }
            let effectful = f.blocks.iter().any(|b| {
                b.insts.iter().any(|inst| op_is_effectful(f, &inst.op, &pure, &program.pure_natives))
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

/// The functions that cannot be shown to terminate, and so are effectful.
///
/// **Non-termination is an effect.** This file already holds that a trap is one, on the
/// grounds that ending the program is as observable as output; a program that *never* ends
/// is observable by exactly the same argument, and it is the one the corpus harness's
/// 60s/10s timeouts exist to catch. Without this, DCE deleted a call to a pure function
/// that loops forever, and a program that should hang printed its next line and exited 0.
///
/// Termination is undecidable, so this is the cheap conservative approximation and nothing
/// more: a function might not terminate if its own CFG has a **back edge**, or if it sits
/// on a **cycle in the call graph** (self-recursion included). Everything else is a finite
/// straight-line walk over finitely many blocks and does terminate. Both directions of
/// being wrong are unequal here — a loop wrongly called diverging costs one missed
/// deletion; a diverging call wrongly deleted changes what the program does — so this
/// leans the cheap way, as the rest of the file does.
///
/// The call-graph half is why the purity fixpoint's optimism is not enough on its own. It
/// starts every function pure so a recursive one stays pure while its own body is read,
/// which is exactly the case that has no termination argument: `fn a() { b() }` and
/// `fn b() { a() }` reach a fixpoint at "both pure" and neither ever returns.
///
/// `Op::CallClosure` needs no arm: an indirect call is already unconditionally effectful.
fn may_diverge(program: &Program) -> HashSet<String> {
    let mut out: HashSet<String> =
        program.funcs.iter().filter(|f| has_back_edge(f)).map(|f| f.name.clone()).collect();

    // Direct-call edges, restricted to callees the program defines. A call to anything
    // else is effectful already, by `op_is_effectful`'s unknown-callee rule.
    let mut calls: HashMap<&str, Vec<&str>> = HashMap::new();
    for f in &program.funcs {
        let known: Vec<&str> = f
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter_map(|inst| match &inst.op {
                Op::Call { func, .. } => Some(func.as_str()),
                _ => None,
            })
            .collect();
        calls.insert(f.name.as_str(), known);
    }
    let defined: HashSet<&str> = program.funcs.iter().map(|f| f.name.as_str()).collect();

    // A function is on a cycle exactly when it can reach itself. Done per function rather
    // than by an SCC decomposition because the call graph is small and "reaches itself" is
    // the property, stated directly, with nothing to get subtly wrong.
    for f in &program.funcs {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut stack: Vec<&str> = calls[f.name.as_str()].clone();
        while let Some(n) = stack.pop() {
            if n == f.name {
                out.insert(f.name.clone());
                break;
            }
            if !defined.contains(n) || !seen.insert(n) {
                continue;
            }
            stack.extend(calls[n].iter().copied());
        }
    }
    out
}

/// Whether the function's CFG has a back edge — an edge to a block already on the current
/// DFS path, which is what a loop is. Iterative, so a deeply nested function cannot blow
/// the compiler's stack; `on_path` is the grey set and `done` the black one.
fn has_back_edge(f: &Func) -> bool {
    enum Step {
        Enter(BlockId),
        Leave(BlockId),
    }
    let mut done: HashSet<BlockId> = HashSet::new();
    let mut on_path: HashSet<BlockId> = HashSet::new();
    let mut stack = vec![Step::Enter(f.entry)];
    while let Some(step) = stack.pop() {
        match step {
            Step::Leave(id) => {
                on_path.remove(&id);
                done.insert(id);
            }
            Step::Enter(id) => {
                if on_path.contains(&id) {
                    return true;
                }
                if done.contains(&id) || id.0 as usize >= f.blocks.len() {
                    continue;
                }
                on_path.insert(id);
                stack.push(Step::Leave(id));
                for s in successors(&f.block(id).term) {
                    stack.push(Step::Enter(s));
                }
            }
        }
    }
    false
}

/// The blocks a terminator can transfer to. Local to this file rather than shared with
/// `opt`: this one only ever feeds a reachability walk, and duplicates do not matter to it.
fn successors(term: &Term) -> Vec<BlockId> {
    match term {
        Term::Jump(t) => vec![t.to],
        Term::Branch { then, els, .. } => vec![then.to, els.to],
        Term::Switch { arms, default, .. } => {
            arms.iter().map(|(_, t)| t.to).chain(std::iter::once(default.to)).collect()
        }
        Term::Ret(_) | Term::Throw(_) | Term::Unreachable => vec![],
    }
}

/// Whether an op has an effect that must be preserved -- so DCE may not drop it even if
/// its result is unused, and CSE may not share it. `pure` maps each function to whether
/// it is pure.
///
/// The catch-all `false` arm is what makes this cheap and also what makes it a place to be
/// careful: allocation, projection, arithmetic and the comparison ops are genuinely pure
/// functions of their operands, and `Retain`/`Release` land here too. Those two are never
/// at risk from DCE despite the verdict, because they have no result and DCE only
/// considers instructions whose result is unused — and refcount insertion runs after the
/// optimiser regardless. `throw` is a terminator, not an `Op`, so it has no arm here and
/// is never a deletion candidate.
pub fn op_is_effectful(
    f: &Func,
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
        // Indexing traps -- out of bounds for a list, absent key for a map -- and a trap
        // ends the program, which is as observable as an effect gets. Deleting one because
        // nobody reads the element is deleting the check: `xs[10]` as a statement ran
        // clean past the end of a three-element list.
        Op::Index { .. } => true,
        // i64 arithmetic can trap, so it is not eliminable. The operand repr is what
        // decides it: the f64 forms follow IEEE and produce an infinity or a NaN rather
        // than trapping, so they stay pure and stay eliminable. That distinction is worth
        // the lookup — calling all arithmetic effectful would make almost every function
        // effectful and leave DCE with nothing it may remove, while calling it all pure
        // deleted `1 / 0`.
        //
        // Which of these actually trap, precisely, because two earlier versions of this
        // comment got it wrong in opposite directions and `opt::fold_int` was written
        // against one of them: per `runtime/src/arith.c` and `docs/decisions.md`, **only
        // `Div` and `Rem` trap** (zero divisor, and `INT64_MIN / -1` whose quotient is not
        // representable). `+`, `-`, `*` and unary `-` **wrap** — two's complement, no trap,
        // which is what `-fwrapv` and the `neon_i64_*` unsigned round-trip buy.
        //
        // `Add`/`Sub`/`Mul`/`Neg` are nonetheless listed, and that is a conservative choice
        // rather than a claim about traps: it costs missed deletions of dead wrapping
        // arithmetic and nothing else. Narrowing this list to `Div | Rem` is a legitimate
        // improvement, but it is not free — it hands DCE a much larger set of deletable
        // calls, so it wants the same evidence any widening of DCE wants, not a comment
        // saying it looks safe.
        Op::Prim(
            PrimOp::Add | PrimOp::Sub | PrimOp::Mul | PrimOp::Div | PrimOp::Rem | PrimOp::Neg,
            operands,
        ) => operands.iter().any(|&v| matches!(f.value_repr(v), Repr::I64)),
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
    fn io_is_effectful_and_reaches_its_callers() {
        let e = analyze_src(
            "@native(\"neon_io_println\") fn println(s: str)
             fn shout(s: str) { println(s); }
             fn calls_io(s: str) { shout(s); }",
        );
        assert_eq!(e.get("shout"), Some(&false), "calls io -> effectful");
        assert_eq!(e.get("calls_io"), Some(&false), "reaches io transitively");
    }

    /// `i64` arithmetic is effectful, so a function doing it is effectful and a call to it
    /// may not be deleted for having an unused result. `f64` follows IEEE and produces an
    /// infinity or a NaN instead of trapping, so it stays pure and stays eliminable.
    ///
    /// The name this test used to carry -- `i64_arithmetic_traps_and_f64_does_not` -- was
    /// false for the case it asserts. `x + x` **wraps**; only `Div` and `Rem` trap. The
    /// classification is conservative, not a trap claim; see `op_is_effectful`.
    ///
    /// The distinction is worth the operand check. Calling all arithmetic effectful would
    /// make almost every function effectful and leave dead-code elimination with nothing
    /// to remove; calling it all pure deleted `1 / 0`.
    #[test]
    fn i64_arithmetic_is_effectful_and_f64_is_not() {
        let e = analyze_src(
            "fn double(x: i64) -> i64 { x + x }
             fn scale(x: f64) -> f64 { x * 2.0 }
             fn compare(a: i64, b: i64) -> bool { a < b }",
        );
        assert_eq!(e.get("double"), Some(&false), "i64 `+` is listed conservatively");
        assert_eq!(e.get("scale"), Some(&true), "f64 arithmetic cannot trap");
        assert_eq!(e.get("compare"), Some(&true), "a comparison cannot trap");
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

    /// Non-termination is an effect. A function that might not terminate cannot be pure,
    /// or DCE deletes a call to it and a program that must hang runs on.
    ///
    /// Termination is undecidable, so `may_diverge` approximates it two ways and both are
    /// exercised here: `loops` has a back edge in its own CFG, while `ping`/`pong` are each
    /// straight-line but sit on a cycle in the call graph. The second is the one the purity
    /// fixpoint's optimism cannot catch on its own -- it starts every function pure so a
    /// recursive one stays pure while its own body is read, and `ping`/`pong` reach a
    /// fixpoint at "both pure" while neither ever returns.
    ///
    /// `straight` is the control: no loop, no recursion, so it stays pure and stays
    /// eliminable. Without it this test would pass just as well if `may_diverge` returned
    /// every function in the program.
    #[test]
    fn a_function_that_might_not_terminate_is_effectful() {
        let e = analyze_src(
            "fn loops(n: f64) -> f64 { let x = n; while x < 100.0 { x = x * 0.0; } x }
             fn ping(n: f64) -> f64 { pong(n) }
             fn pong(n: f64) -> f64 { ping(n) }
             fn straight(n: f64) -> f64 { n * 2.0 }",
        );
        assert_eq!(e.get("loops"), Some(&false), "a CFG back edge is not provably finite");
        assert_eq!(e.get("ping"), Some(&false), "mutual recursion is not provably finite");
        assert_eq!(e.get("pong"), Some(&false), "mutual recursion is not provably finite");
        assert_eq!(e.get("straight"), Some(&true), "no loop and no recursion: still pure");
    }
}
