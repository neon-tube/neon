//! The `stale_thread` lint: a read of pre-advance state after a threading call captured its
//! successor under another name. These pin what fires and, more importantly, what does NOT --
//! the whole stdlib threads generators and readers and must stay silent.
use neon_compiler::lint::Lint;
use neon_compiler::typecheck::env::Unit;
use neon_compiler::typecheck::{check::check_all, Env};
use neon_compiler::{lexer, parser};
use std::path::Path;

/// The number of `stale_thread` warnings a program raises, checked against the real stdlib.
fn stale_thread_warnings(src: &str) -> usize {
    let std_sources = stdlib_sources();
    let tokens = lexer::lex(src).expect("lexes");
    let (module, perrs) = parser::parse(&tokens, src.len());
    assert!(perrs.is_empty(), "parse errors: {perrs:?}");
    let module = module.expect("module");
    let cfg = neon_compiler::expand::Config::for_host(std::iter::empty());
    let (mut module, _m, eerrs) = neon_compiler::expand::expand(module, &cfg);
    assert!(eerrs.is_empty(), "expand errors: {eerrs:?}");

    let (std_modules, next_id) =
        neon_compiler::stdlib::parse_from(&std_sources, 0).expect("stdlib parses");
    neon_compiler::ast::number_exprs_from(&mut module, next_id);

    let mut modules: Vec<(Vec<String>, &_)> =
        std_modules.iter().map(|(p, m)| (p.clone(), m)).collect();
    modules.push((Vec::new(), &module));
    let mut env = Env::build_with(&modules, Unit::RootApplication);
    assert!(env.errors().is_empty(), "env errors: {:?}", env.errors());
    let (result, errs) = check_all(&mut env, &modules);
    assert!(errs.is_empty(), "check errors: {errs:?}");
    // Only warnings raised against the PROGRAM module (empty path), not the stdlib's own.
    result
        .warnings
        .iter()
        .filter(|w| w.lint == Lint::StaleThread && w.module.is_empty())
        .count()
}

#[test]
fn fires_on_a_stale_read_of_pre_advance_state() {
    // `int` advances the generator into `rng2`, then the next call reads the OLD `rng`.
    let n = stale_thread_warnings(
        r##"
use std::io;
use std::random;
fn main() {
    let rng = random::new(1);
    let (rng2, x) = random::int(rng, 0, 10);
    let (rng3, y) = random::int(rng, 0, 10);
    io::println("#{x} #{y} #{rng2 == rng2} #{rng3 == rng3}");
}
"##,
    );
    assert_eq!(n, 1, "the stale read of `rng` should warn exactly once");
}

#[test]
fn silent_when_allowed() {
    // The escape hatch, for the rare deliberate read of earlier state.
    let n = stale_thread_warnings(
        r##"
use std::io;
use std::random;
@allow(stale_thread)
fn main() {
    let rng = random::new(1);
    let (rng2, x) = random::int(rng, 0, 10);
    let (rng3, y) = random::int(rng, 0, 10);
    io::println("#{x} #{y} #{rng2 == rng2} #{rng3 == rng3}");
}
"##,
    );
    assert_eq!(n, 0, "@allow(stale_thread) suppresses it");
}

#[test]
fn silent_on_the_same_name_rebind() {
    // The correct idiom: the advance shadows the old name, so no stale name exists.
    let n = stale_thread_warnings(
        r##"
use std::io;
use std::random;
fn main() {
    let rng = random::new(1);
    let (rng, x) = random::int(rng, 0, 10);
    let (rng, y) = random::int(rng, 0, 10);
    io::println("#{x} #{y}");
}
"##,
    );
    assert_eq!(n, 0);
}

#[test]
fn silent_on_a_deliberate_fork_with_underscores() {
    // Forking one generator into two draws is legitimate, and spelled with `_`-names.
    let n = stale_thread_warnings(
        r##"
use std::io;
use std::random;
fn main() {
    let held = random::new(7);
    let (_r1, x1) = random::next(held);
    let (_r2, x2) = random::next(held);
    io::println("#{x1 == x2}");
}
"##,
    );
    assert_eq!(n, 0);
}

#[test]
fn silent_on_capture_then_assign_back() {
    // The temp-and-reassign spelling of the same rebind: `rng` is written before any read.
    let n = stale_thread_warnings(
        r##"
use std::io;
use std::random;
fn main() {
    let rng = random::new(1);
    let (r2, x) = random::int(rng, 0, 10);
    rng = r2;
    let (r3, y) = random::int(rng, 0, 10);
    io::println("#{x} #{y} #{r3 == r3}");
}
"##,
    );
    assert_eq!(n, 0);
}

#[test]
fn silent_on_sequential_fresh_names() {
    // A chain that threads each successor forward under a new name, never reusing an old one.
    let n = stale_thread_warnings(
        r##"
use std::io;
use std::random;
fn main() {
    let r0 = random::new(1);
    let (r1, a) = random::int(r0, 0, 10);
    let (r2, b) = random::int(r1, 0, 10);
    io::println("#{a} #{b} #{r2 == r2}");
}
"##,
    );
    assert_eq!(n, 0);
}

#[test]
fn the_stdlib_threads_cleanly() {
    // Every generator op, reader and channel primitive in the stdlib threads state; if the
    // lint fired on any of it, that would be a false positive on code known to be correct.
    let std_sources = stdlib_sources();
    let (std_modules, _next) =
        neon_compiler::stdlib::parse_from(&std_sources, 0).expect("stdlib parses");
    let modules: Vec<(Vec<String>, &_)> =
        std_modules.iter().map(|(p, m)| (p.clone(), m)).collect();
    let mut env = Env::build_with(&modules, Unit::Library);
    assert!(env.errors().is_empty(), "env errors: {:?}", env.errors());
    let (result, _errs) = check_all(&mut env, &modules);
    let stale: Vec<_> = result
        .warnings
        .iter()
        .filter(|w| w.lint == Lint::StaleThread)
        .map(|w| format!("{:?}:{:?}", w.module, w.span))
        .collect();
    assert!(stale.is_empty(), "stdlib tripped stale_thread: {stale:?}");
}

/// A whole-corpus false-positive sweep. `#[ignore]` because it re-checks every passing
/// program (slow) and the stdlib test already guards the densest threading code; run it with
/// `--ignored` after touching the lint. Any hit is a real false positive to fix or `@allow`.
#[test]
#[ignore]
fn the_corpus_threads_cleanly() {
    let lang = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/lang");
    let listed = std::fs::read_to_string(lang.join("expected-pass.txt")).expect("readable");
    let mut hits = Vec::new();
    for rel in listed
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        let Ok(src) = std::fs::read_to_string(lang.join(rel)) else {
            continue;
        };
        // compile-fail files are not well-formed programs; skip them.
        if src.contains("//@ compile-fail") {
            continue;
        }
        let n = std::panic::catch_unwind(|| stale_thread_warnings(&src)).unwrap_or(0);
        if n > 0 {
            hits.push(format!("{rel}: {n}"));
        }
    }
    assert!(hits.is_empty(), "corpus tripped stale_thread: {hits:?}");
}

fn stdlib_sources() -> Vec<(String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../stdlib");
    let mut out = Vec::new();
    collect(&root, &root, &mut out);
    out
}
fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
    for entry in std::fs::read_dir(dir).expect("readable") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            collect(root, &path, out);
        } else if path.extension().is_some_and(|e| e == "neon") {
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, std::fs::read_to_string(&path).expect("readable")));
        }
    }
}
