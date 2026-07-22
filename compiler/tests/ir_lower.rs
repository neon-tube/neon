//! Coverage probe and two lowering invariants, run over the REAL pipeline.
//!
//! These guards previously lowered with `libs = &[]`, parsed the stdlib without
//! renumbering (so `ExprId`s collided and stdlib bodies went unchecked), and scanned
//! `f.values()` only. Rebuilt correctly the answers were still 0 — latent, not live —
//! but a guard aimed at a program the compiler never builds proves nothing about the
//! one it does. This harness now mirrors `cli/src/frontend.rs` exactly:
//! stdlib parsed and numbered first (`stdlib::parse_from`), the program numbered after
//! it, `check_all` over the whole compilation, and lowering handed the stdlib as
//! `libs`. The invariant scans cover every repr position a `Func` and a `Program`
//! carry: values, `ret`, `throws`, `env`, `Op::IsVariant::tested`, `program.recursive`
//! and `program.boxed`.

use neon_compiler::ir::lower::lower_module;
use neon_compiler::ir::repr::Repr;
use neon_compiler::ir::ssa::{print, Func, Op, Program};
use neon_compiler::typecheck::env::Unit;
use neon_compiler::typecheck::{check::check_all, Env};
use neon_compiler::{lexer, parser};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn lang_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/lang")
}

fn stdlib_sources() -> Vec<(String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../stdlib");
    let mut sources = Vec::new();
    collect_neon(&root, &root, &mut sources);
    sources
}

fn collect_neon(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
    for entry in std::fs::read_dir(dir).expect("readable") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            collect_neon(root, &path, out);
        } else if path.extension().is_some_and(|e| e == "neon") {
            let rel = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            out.push((rel, std::fs::read_to_string(&path).expect("readable")));
        }
    }
}

fn expected_pass() -> Vec<String> {
    let src = std::fs::read_to_string(lang_root().join("expected-pass.txt")).expect("readable");
    src.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// Compile every passing corpus program the way the cli does, and lower it the way the
/// cli does. Each entry pairs the program with the `any` TyId from its own `Env` (type
/// ids are per-env, so the guards below cannot share one).
fn lowered_corpus() -> Vec<(String, Program, neon_compiler::typecheck::types::TyId)> {
    let std_sources = stdlib_sources();
    let mut out = Vec::new();
    for rel in expected_pass() {
        let Ok(src) = std::fs::read_to_string(lang_root().join(&rel)) else { continue };
        let Ok(tokens) = lexer::lex(&src) else { continue };
        let (module, perrs) = parser::parse(&tokens, src.len());
        if !perrs.is_empty() {
            continue;
        }
        let Some(mut module) = module else { continue };

        // The real pipeline: stdlib numbered from 0, the program numbered after it, so
        // one `TypecheckResult` covers both and stdlib bodies are checked and lowered.
        let (std_modules, next_id) =
            neon_compiler::stdlib::parse_from(&std_sources, 0).expect("stdlib parses");
        neon_compiler::ast::number_exprs_from(&mut module, next_id);

        let mut modules: Vec<(Vec<String>, &_)> =
            std_modules.iter().map(|(p, m)| (p.clone(), m)).collect();
        modules.push((Vec::new(), &module));
        let mut env = Env::build_with(&modules, Unit::RootApplication);
        if !env.errors().is_empty() {
            continue;
        }
        let (result, errs) = check_all(&mut env, &modules);
        if !errs.is_empty() {
            continue;
        }
        let libs: Vec<(Vec<String>, &_)> =
            std_modules.iter().map(|(p, m)| (p.clone(), m)).collect();
        let program = lower_module(&env, &result, &module, &libs);
        out.push((rel, program, env.solver.t.any()));
    }
    out
}

#[test]
fn lower_the_corpus_and_report_gaps() {
    let mut todos: BTreeMap<String, usize> = BTreeMap::new();
    let mut files = 0;
    let mut clean = 0;

    for (_rel, program, _) in lowered_corpus() {
        files += 1;
        let ir = print::program(&program);
        let mut file_todos = 0;
        for line in ir.lines() {
            if let Some(i) = line.find("<todo: ") {
                let rest = &line[i + 7..];
                if let Some(end) = rest.find('>') {
                    *todos.entry(rest[..end].to_string()).or_default() += 1;
                    file_todos += 1;
                }
            }
        }
        if file_todos == 0 {
            clean += 1;
        }
    }

    eprintln!("\n=== IR lowering coverage ===");
    eprintln!("files lowered: {files}, fully lowered (no todos): {clean}");
    eprintln!("remaining unhandled forms:");
    let mut sorted: Vec<_> = todos.iter().collect();
    sorted.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
    for (what, n) in sorted {
        eprintln!("  {n:>5}  {what}");
    }
    assert!(files > 100, "expected to lower most of the corpus, got {files}");
    assert_eq!(clean, files, "every checkable corpus program should lower fully");
}

/// Every repr a lowered function carries, with a label saying where it lives. Values,
/// the return, the throws slot, a lambda's environment, and each `Op::IsVariant`'s
/// resolved `tested` — the positions the original scan missed are exactly where the
/// historical bugs lived (`lower_try`'s handler param, `wrap_throwing`'s error repr).
fn func_reprs(f: &Func) -> Vec<(String, &Repr)> {
    let mut out: Vec<(String, &Repr)> = Vec::new();
    for v in f.values() {
        out.push((format!("%{}", v.0), f.value_repr(v)));
    }
    out.push(("ret".into(), &f.ret));
    if let Some(t) = &f.throws {
        out.push(("throws".into(), t));
    }
    if let Some(e) = &f.env {
        out.push(("env".into(), e));
    }
    for b in &f.blocks {
        for inst in &b.insts {
            if let Op::IsVariant { tested: Some(t), .. } = &inst.op {
                out.push(("IsVariant::tested".into(), t));
            }
        }
    }
    out
}

/// `any` may only ever come from source. It is legitimate when written — `throws any`
/// asks for erasure and gets a box — but the compiler must never *invent* it as a
/// fallback, a default, or an unhandled case. That is how the previous compiler died,
/// and it recurred twice here: `lower_try` hardcoded `Repr::Any` for a handler's error
/// parameter, and `wrap_throwing` was handed the callee's declared error type and threw
/// it away. Both produced a value whose repr was `Any` while its *type* was not `any`,
/// which is exactly what this asserts cannot happen.
///
/// This has to run over the lowered IR rather than `repr_of`: neither bug went through
/// `repr_of`, so a check on the representation map alone would have missed both.
#[test]
fn any_never_appears_unless_the_source_type_is_any() {
    let mut checked = 0;
    let mut offenders: Vec<String> = Vec::new();

    for (rel, program, top) in lowered_corpus() {
        checked += 1;
        for f in &program.funcs {
            for v in f.values() {
                if matches!(f.value_repr(v), Repr::Any) && f.value_ty(v) != top {
                    offenders.push(format!("{rel}: {} %{}", f.name, v.0));
                }
            }
        }
    }

    assert!(checked > 100, "expected to lower most of the corpus, got {checked}");
    assert!(
        offenders.is_empty(),
        "the compiler invented `any` for {} value(s) whose source type is not `any`:\n  {}",
        offenders.len(),
        offenders.iter().take(20).cloned().collect::<Vec<_>>().join("\n  "),
    );
}

/// The same invariant as above, in its other spelling. Monomorphisation means a lowered
/// program contains only concrete functions and instances — never an uninstantiated
/// template — so a surviving `Repr::Var` is always a substitution someone forgot.
///
/// The `any` guard does not catch it, and the gap cost three bugs in the same two
/// functions: `lower_try` built a handler's error parameter with `repr_of` instead of
/// `repr_of_ty`, and `wrap_throwing` did the same with the callee's `throws`. Under
/// `throws E` both produced `Var("E")`, which the call site then built a tagged result
/// from while the instance had substituted — one call, two layouts for its result.
///
/// `c_type` now panics on a `Var`, which covers every compiled program rather than only
/// the corpus. This reports the offending function and position instead, which is what
/// makes such a bug findable — and it covers every repr position a function and a
/// program carry, including `program.recursive` and `program.boxed`.
#[test]
fn no_type_variable_survives_lowering() {
    let mut checked = 0;
    let mut offenders: Vec<String> = Vec::new();

    for (rel, program, _) in lowered_corpus() {
        checked += 1;
        for f in &program.funcs {
            for (wher, r) in func_reprs(f) {
                if let Some(var) = unsubstituted_var(r) {
                    offenders.push(format!("{rel}: {} {wher} holds '{var}", f.name));
                }
            }
        }
        for (ty, r) in &program.recursive {
            if let Some(var) = unsubstituted_var(r) {
                offenders.push(format!("{rel}: recursive[{ty:?}] holds '{var}"));
            }
        }
        for (atom, r) in &program.boxed {
            if let Some(var) = unsubstituted_var(r) {
                offenders.push(format!("{rel}: boxed[{atom}] holds '{var}"));
            }
        }
    }

    assert!(checked > 100, "expected to lower most of the corpus, got {checked}");
    assert!(
        offenders.is_empty(),
        "{} repr position(s) kept an unsubstituted type variable, which codegen boxes \
         as `neon_value`:\n  {}",
        offenders.len(),
        offenders.iter().take(20).cloned().collect::<Vec<_>>().join("\n  "),
    );
}

/// The name of the first type variable anywhere inside a repr — nested, since the ones
/// that bite hide in a closure's `throws` or a union's error variant rather than at the
/// top level.
fn unsubstituted_var(r: &Repr) -> Option<String> {
    match r {
        Repr::Var(n) => Some(n.clone()),
        Repr::List(e) | Repr::Nullable(e) => unsubstituted_var(e),
        Repr::Map(k, v) => unsubstituted_var(k).or_else(|| unsubstituted_var(v)),
        Repr::Tuple(rs) | Repr::Union(rs) => rs.iter().find_map(unsubstituted_var),
        Repr::Runtime { args, .. } => args.iter().find_map(unsubstituted_var),
        Repr::Record { fields, .. } => fields.iter().find_map(|(_, r)| unsubstituted_var(r)),
        Repr::Closure { params, throws, ret } => params
            .iter()
            .find_map(unsubstituted_var)
            .or_else(|| unsubstituted_var(throws))
            .or_else(|| unsubstituted_var(ret)),
        _ => None,
    }
}

/// The verifier for `Repr::assignable`, the block-parameter relation. Every
/// predecessor edge in every lowered corpus program: the argument's repr must be
/// assignable to the parameter's.
/// Equality here flagged 9,226 sites (the emitter widens); assignable must flag none —
/// and when a lowering change starts passing something inconvertible, this names the
/// function and edge instead of letting the join read garbage.
#[test]
fn block_arguments_are_assignable_to_their_parameters() {
    use neon_compiler::ir::ssa::Term;
    let mut checked_edges = 0usize;
    let mut offenders: Vec<String> = Vec::new();

    for (rel, program, _) in lowered_corpus() {
        for f in &program.funcs {
            let params: std::collections::HashMap<_, Vec<&Repr>> = f
                .blocks
                .iter()
                .map(|b| (b.id, b.params.iter().map(|&p| f.value_repr(p)).collect()))
                .collect();
            let mut check_target = |t: &neon_compiler::ir::ssa::Target,
                                    offenders: &mut Vec<String>| {
                let Some(ps) = params.get(&t.to) else { return };
                for (i, (&arg, &want)) in t.args.iter().zip(ps.iter()).enumerate() {
                    checked_edges += 1;
                    let got = f.value_repr(arg);
                    if !got.assignable(want) {
                        offenders.push(format!(
                            "{rel}: {} -> block{} arg {i}: {got:?} not assignable to {want:?}",
                            f.name, t.to.0
                        ));
                    }
                }
            };
            for b in &f.blocks {
                match &b.term {
                    Term::Jump(t) => check_target(t, &mut offenders),
                    Term::Branch { then, els, .. } => {
                        check_target(then, &mut offenders);
                        check_target(els, &mut offenders);
                    }
                    Term::Switch { arms, default, .. } => {
                        for (_, t) in arms {
                            check_target(t, &mut offenders);
                        }
                        check_target(default, &mut offenders);
                    }
                    Term::Ret(_) | Term::Throw(_) | Term::Unreachable => {}
                }
            }
        }
    }

    assert!(checked_edges > 5000, "expected thousands of edges, got {checked_edges}");
    assert!(
        offenders.is_empty(),
        "{} block argument(s) violate the assignable relation:\n  {}",
        offenders.len(),
        offenders.iter().take(20).cloned().collect::<Vec<_>>().join("\n  "),
    );
}
