//! Coverage probe: lower every passing corpus program and count the not-yet-lowered
//! forms (the `<todo: ...>` markers). Not a ratchet — a live report of what the lowering
//! still cannot express, so the remaining work is visible.

use neon_compiler::ir::lower::lower_module;
use neon_compiler::ir::ssa::print;
use neon_compiler::typecheck::env::Unit;
use neon_compiler::typecheck::{check::check_module, Env};
use neon_compiler::{lexer, parser};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn lang_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/lang")
}

fn stdlib_modules() -> Vec<(Vec<String>, neon_compiler::ast::Module)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../stdlib");
    let mut sources = Vec::new();
    collect_neon(&root, &root, &mut sources);
    neon_compiler::stdlib::parse(&sources).expect("stdlib parses")
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

/// Lower every corpus program that checks cleanly, pairing each with the `any` TyId from
/// its own `Env` (type ids are per-env, so the guards below cannot share one).
fn lowered_corpus() -> Vec<(String, neon_compiler::ir::ssa::Program, neon_compiler::typecheck::types::TyId)>
{
    let mut out = Vec::new();
    for rel in expected_pass() {
        let Ok(src) = std::fs::read_to_string(lang_root().join(&rel)) else { continue };
        let Ok(tokens) = lexer::lex(&src) else { continue };
        let (module, perrs) = parser::parse(&tokens, src.len());
        if !perrs.is_empty() {
            continue;
        }
        let Some(module) = module else { continue };

        let std_owned = stdlib_modules();
        let mut modules: Vec<(Vec<String>, &_)> =
            std_owned.iter().map(|(p, m)| (p.clone(), m)).collect();
        modules.push((Vec::new(), &module));
        let mut env = Env::build_with(&modules, Unit::RootApplication);
        if !env.errors().is_empty() {
            continue;
        }
        let (result, errs) = check_module(&mut env, &module);
        if !errs.is_empty() {
            continue;
        }
        let program = lower_module(&env, &result, &module, &[]);
        out.push((rel, program, env.solver.t.any()));
    }
    out
}

#[test]
fn lower_the_corpus_and_report_gaps() {
    let mut todos: BTreeMap<String, usize> = BTreeMap::new();
    let mut files = 0;
    let mut clean = 0;

    for rel in expected_pass() {
        let Ok(src) = std::fs::read_to_string(lang_root().join(&rel)) else { continue };
        let Ok(tokens) = lexer::lex(&src) else { continue };
        let (module, perrs) = parser::parse(&tokens, src.len());
        if !perrs.is_empty() {
            continue;
        }
        let Some(module) = module else { continue };

        let std_owned = stdlib_modules();
        let mut modules: Vec<(Vec<String>, &_)> =
            std_owned.iter().map(|(p, m)| (p.clone(), m)).collect();
        modules.push((Vec::new(), &module));

        let mut env = Env::build_with(&modules, Unit::RootApplication);
        if !env.errors().is_empty() {
            continue;
        }
        let (result, errs) = check_module(&mut env, &module);
        if !errs.is_empty() {
            continue;
        }

        files += 1;
        let ir = print::program(&lower_module(&env, &result, &module, &[]));
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
                if matches!(f.value_repr(v), neon_compiler::ir::repr::Repr::Any)
                    && f.value_ty(v) != top
                {
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
/// It is not caught by the `any` guard, and that gap has now cost three bugs in the same
/// two functions. `lower_try` built a handler's error parameter with `repr_of` instead of
/// `repr_of_ty`, and `wrap_throwing` did the same with the callee's `throws`: under
/// `throws E` both produced `Var("E")`, which reaches `c_type`'s `_ => "neon_value"`
/// catch-all and is boxed without complaint. The result disagreed with the instance,
/// which *had* substituted — one call, two layouts for its result.
///
/// Both spellings share a sink, and that is the real lesson: an unpinned repr becomes
/// `neon_value` silently. Until that catch-all is closed, this guard is what stands in
/// for it — and unlike a check on `repr_of`, it runs over the lowered IR, because none of
/// these bugs went through `repr_of`.
#[test]
fn no_type_variable_survives_lowering() {
    let mut checked = 0;
    let mut offenders: Vec<String> = Vec::new();

    for (rel, program, _) in lowered_corpus() {
        checked += 1;
        for f in &program.funcs {
            for v in f.values() {
                if let Some(var) = unsubstituted_var(f.value_repr(v)) {
                    offenders.push(format!("{rel}: {} %{} holds '{var}", f.name, v.0));
                }
            }
        }
    }

    assert!(checked > 100, "expected to lower most of the corpus, got {checked}");
    assert!(
        offenders.is_empty(),
        "{} value(s) kept an unsubstituted type variable, which codegen boxes as \
         `neon_value`:\n  {}",
        offenders.len(),
        offenders.iter().take(20).cloned().collect::<Vec<_>>().join("\n  "),
    );
}

/// The name of the first type variable anywhere inside a repr — nested, since the ones
/// that bite hide in a closure's `throws` or a union's error variant rather than at the
/// top level.
fn unsubstituted_var(r: &neon_compiler::ir::repr::Repr) -> Option<String> {
    use neon_compiler::ir::repr::Repr;
    match r {
        Repr::Var(n) => Some(n.clone()),
        Repr::List(e) | Repr::Nullable(e) => unsubstituted_var(e),
        Repr::Map(k, v) => unsubstituted_var(k).or_else(|| unsubstituted_var(v)),
        Repr::Tuple(rs) | Repr::Union(rs) => rs.iter().find_map(unsubstituted_var),
        Repr::Record { fields, .. } => fields.iter().find_map(|(_, r)| unsubstituted_var(r)),
        Repr::Closure { params, throws, ret } => params
            .iter()
            .find_map(unsubstituted_var)
            .or_else(|| unsubstituted_var(throws))
            .or_else(|| unsubstituted_var(ret)),
        _ => None,
    }
}
