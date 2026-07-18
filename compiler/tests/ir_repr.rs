//! The representation map is **total**: every type the checker assigns to an expression,
//! across every program in the corpus, maps to a `Repr` with no unknown case. This is
//! the mechanical guarantee that the graveyard's `Erased` cannot come back — if a type
//! had no representation, `repr_of` would have nowhere to go, and this test walks every
//! real one the front end produces.

use neon_compiler::ir::repr::{repr_of, Repr};
use neon_compiler::typecheck::env::Unit;
use neon_compiler::typecheck::{check::check_module, Env};
use neon_compiler::{lexer, parser};
use std::path::{Path, PathBuf};

fn lang_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/lang")
}

fn stdlib_modules() -> Vec<(Vec<String>, neon_compiler::ast::Module)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../stdlib");
    let mut sources = Vec::new();
    collect_neon(&root, &root, &mut sources);
    neon_compiler::stdlib::parse(&sources).expect("the stdlib parses")
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

/// Accumulates which `Repr` shapes were seen, to prove the map covers the real variety
/// rather than only the easy scalar cases.
#[derive(Default)]
struct Seen {
    total: usize,
    list: bool,
    map: bool,
    record: bool,
    tuple: bool,
    union: bool,
    nullable: bool,
    closure: bool,
    tag: bool,
}

fn note(seen: &mut Seen, r: &Repr) {
    seen.total += 1;
    match r {
        Repr::List(_) => seen.list = true,
        Repr::Map(_, _) => seen.map = true,
        Repr::Record { .. } => seen.record = true,
        Repr::Tuple(_) => seen.tuple = true,
        Repr::Union(_) => seen.union = true,
        Repr::Nullable(_) => seen.nullable = true,
        Repr::Closure { .. } => seen.closure = true,
        Repr::Tag => seen.tag = true,
        _ => {}
    }
}

#[test]
fn every_corpus_type_has_a_representation() {
    let mut seen = Seen::default();
    let mut files = 0;

    for rel in expected_pass() {
        // A compile-fail entry is recorded too; only the ones that check clean produce a
        // usable `TypecheckResult`.
        let src = match std::fs::read_to_string(lang_root().join(&rel)) {
            Ok(s) => s,
            Err(_) => continue,
        };
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
        for (_, ty) in result.types() {
            // The guarantee: this call is total — it returns a `Repr` for every type,
            // never a panic and never an "unknown". A concrete program yields concrete
            // reprs; a generic body may yield `Var`, which monomorphisation removes.
            let r = repr_of(&env.solver.t, ty);
            note(&mut seen, &r);
        }
    }

    eprintln!(
        "files={files} total={} list={} map={} record={} tuple={} union={} nullable={} closure={} tag={}",
        seen.total, seen.list, seen.map, seen.record, seen.tuple, seen.union, seen.nullable,
        seen.closure, seen.tag
    );
    assert!(files > 100, "expected to cover most of the corpus, only ran {files} files");
    assert!(seen.total > 1000, "expected many types, saw {}", seen.total);
    // Breadth: the interesting shapes must actually occur, or the map is only being
    // exercised on scalars and the totality claim is hollow.
    assert!(seen.record, "no record repr seen");
    assert!(seen.union, "no union repr seen");
    assert!(seen.list, "no list repr seen");
    assert!(seen.map, "no map repr seen");
    assert!(seen.nullable, "no nullable repr seen");
    assert!(seen.closure, "no closure repr seen");
    assert!(seen.tag, "no atom-tag repr seen");
}
