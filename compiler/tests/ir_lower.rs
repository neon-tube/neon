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
        let ir = print::program(&lower_module(&env, &result, &module));
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
