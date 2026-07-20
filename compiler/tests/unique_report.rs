//! What the sole-ownership query finds across the corpus.
//!
//! Not an assertion about performance and not yet driving anything — it exists so the
//! analysis can be read against real programs *before* an optimisation is built on it. A
//! wrong answer here does not crash; it mutates a list somebody else is holding. So the
//! order is: find out how often it fires and on what, then decide.
//!
//! Run with `cargo test -p neon-compiler --test unique_report -- --nocapture`.

use neon_compiler::ir::{self, Stage};
use neon_compiler::typecheck::env::Unit;
use neon_compiler::typecheck::{check::check_all, Env};
use neon_compiler::{lexer, parser};
use std::path::{Path, PathBuf};

fn lang_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/lang")
}

fn corpus() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).expect("readable") {
            let p = e.expect("entry").path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "neon") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(&lang_root(), &mut out);
    // The benchmark too: it is the program the analysis was designed against, so a query
    // that does not fire on it is telling us something about the query.
    let bench = Path::new(env!("CARGO_MANIFEST_DIR")).join("../bench");
    if bench.is_dir() {
        walk(&bench, &mut out);
    }
    if let Ok(extra) = std::env::var("UNIQ_EXTRA") {
        let d = PathBuf::from(extra);
        if d.is_dir() {
            walk(&d, &mut out);
        }
    }
    out.sort();
    out
}

fn stdlib_modules() -> (Vec<(Vec<String>, neon_compiler::ast::Module)>, u32) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../stdlib");
    fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
        for e in std::fs::read_dir(dir).expect("readable") {
            let p = e.expect("entry").path();
            if p.is_dir() {
                collect(root, &p, out);
            } else if p.extension().is_some_and(|x| x == "neon") {
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                out.push((rel, std::fs::read_to_string(&p).expect("readable")));
            }
        }
    }
    let mut sources = Vec::new();
    collect(&root, &root, &mut sources);
    neon_compiler::stdlib::parse_from(&sources, 0).expect("stdlib parses")
}

#[test]
fn report() {
    let (std_owned, next_id) = stdlib_modules();
    let mut total = 0usize;
    let mut files_with = 0usize;
    let mut rows: Vec<String> = Vec::new();

    for path in corpus() {
        let src = std::fs::read_to_string(&path).expect("readable");
        let Ok(tokens) = lexer::lex(&src) else { continue };
        let (module, perrs) = parser::parse(&tokens, src.len());
        if !perrs.is_empty() {
            continue;
        }
        let Some(mut module) = module else { continue };
        neon_compiler::ast::number_exprs_from(&mut module, next_id);
        let mut modules: Vec<(Vec<String>, &_)> =
            std_owned.iter().map(|(p, m)| (p.clone(), m)).collect();
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
            std_owned.iter().map(|(p, m)| (p.clone(), m)).collect();
        // BEFORE refcounting, deliberately. The refcount pass inserts `retain` on the
        // very value the chain carries -- bookkeeping balanced by a matching release, not
        // a second live reference -- and no sound reading of a bare `Op::Retain` can tell
        // the two apart. Asking at `Stage::Optimised` means the question is put to the IR
        // that still describes ownership rather than its accounting.
        let program = ir::compile(&env, &result, &module, &libs, Stage::Optimised);
        let found = ir::unique::candidates(&program);
        if std::env::var("UNIQ_DEBUG").is_ok() {
            for f in &program.funcs {
                let sets = ir::unique::debug_sets(f);
                let backs = ir::unique::debug_back_edges(f);
                if !sets.is_empty() {
                    println!(
                        "  [dbg] {}: {} set call(s), {} back edge(s), headers {:?}",
                        f.name, sets.len(), backs.len(),
                        backs.iter().map(|(_, h)| h.0).collect::<Vec<_>>()
                    );
                    for (r, a) in &sets {
                        println!("        set: {:?} <- list {:?}", r, a);
                    }
                }
            }
        }
        if !found.is_empty() {
            files_with += 1;
            total += found.len();
            let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
            let name = path.strip_prefix(&root).unwrap_or(&path).to_string_lossy().to_string();
            for c in &found {
                rows.push(format!("  {name}: {} (block {:?}, {} writes)", c.func, c.header, c.writes));
            }
        }
    }

    println!("\n=== sole-ownership candidates ===");
    for r in &rows {
        println!("{r}");
    }
    println!("total: {total} candidate(s) across {files_with} corpus file(s)\n");
}
