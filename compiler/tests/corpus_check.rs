//! The corpus, through the checker.
//!
//! `tests/lang/**/*.neon` is the language specification. This runs the `//@`
//! directives in it against the front end and holds the results to the
//! `expected-pass.txt` ratchet described in `tests/lang/README.md`.
//!
//! # What "pass" means here, today
//!
//! Less than the README says, and the gap matters. The README describes a
//! harness that "compiles it, links it, runs the binary, and diffs stdout and
//! the exit code". There is no backend: nothing is codegen'd, linked or run. So
//! this checks only what the front end can answer:
//!
//! - a `//@ compile-fail` file passes when compilation fails *and* every
//!   `//@ error-contains:` substring appears in the rendered diagnostics;
//! - any other file passes when it checks clean. That is all. Its `.stdout` is
//!   never read and its `//@ exit:` is never verified — both are validated as
//!   well-formed and then ignored.
//!
//! When the backend lands, "pass" strengthens to include stdout and the exit
//! code, and every file already in `expected-pass.txt` must be re-verified
//! against that stronger meaning. A green run of this test is not evidence that
//! any Neon program produces the right answer.

use neon_compiler::diagnostic::Renderer;
use neon_compiler::typecheck::env::Unit;
use neon_compiler::typecheck::Env;
use neon_compiler::{lexer, parser};
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/lang")
}

fn corpus() -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect(&root(), &mut out);
    out.sort();
    assert!(!out.is_empty(), "no corpus at {}", root().display());
    out
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("corpus directory is readable") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "neon") {
            out.push(path);
        }
    }
}

/// Corpus-relative, slash-separated: the name the ratchet and the reports use.
fn name(path: &Path) -> String {
    path.strip_prefix(root())
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

struct Directives {
    compile_fail: bool,
    error_contains: Vec<String>,
}

/// Parse the `//@` lines, and reject a file whose directives cannot mean what
/// they say. The corpus is the spec, so a test that lies is worse than one that
/// fails.
fn directives(path: &Path, src: &str) -> Result<Directives, String> {
    let mut d = Directives { compile_fail: false, error_contains: Vec::new() };
    let mut exit = false;

    // Only the leading comment block: the one ending at the first line that is
    // neither blank nor a `//` comment. A `//@` later in the file is prose.
    for line in src.lines() {
        let line = line.trim();
        if !line.is_empty() && !line.starts_with("//") {
            break;
        }
        let Some(rest) = line.strip_prefix("//@") else {
            continue;
        };
        let rest = rest.trim();
        if rest == "compile-fail" {
            d.compile_fail = true;
        } else if let Some(s) = rest.strip_prefix("error-contains:") {
            let s = s.trim();
            if s.is_empty() {
                return Err(format!("{}: `//@ error-contains:` with no substring", name(path)));
            }
            d.error_contains.push(s.to_string());
        } else if let Some(s) = rest.strip_prefix("exit:") {
            if s.trim().parse::<i32>().is_err() {
                return Err(format!("{}: `//@ exit: {}` is not a number", name(path), s.trim()));
            }
            exit = true;
        } else {
            return Err(format!("{}: unknown directive `//@ {rest}`", name(path)));
        }
    }

    let stdout = path.with_extension("stdout").exists();
    if d.compile_fail {
        if exit {
            return Err(format!("{}: `//@ exit:` on a compile-fail file; it never runs", name(path)));
        }
        if stdout {
            return Err(format!("{}: compile-fail file has a .stdout; it never runs", name(path)));
        }
    } else {
        if !d.error_contains.is_empty() {
            return Err(format!("{}: `//@ error-contains:` without `//@ compile-fail`", name(path)));
        }
        if !stdout {
            return Err(format!("{}: no .stdout; required unless compile-fail", name(path)));
        }
    }
    Ok(d)
}

/// Why a file does not check.
struct Failure {
    /// The diagnostic messages alone. `error-contains` matches THESE, never the
    /// rendered form — that carries the file path and echoes the offending source
    /// line, so any substring occurring in either matches no matter what the
    /// compiler actually said. `main_throws_clause_is_fixed.neon` asserting
    /// `error-contains: main` passes on the strength of its own filename.
    messages: Vec<String>,
    /// What a user would see. For the failure report only.
    rendered: String,
}

impl Failure {
    fn of<'a>(
        path: &Path,
        src: &str,
        errs: impl Iterator<Item = (std::ops::Range<usize>, String)> + 'a,
    ) -> Failure {
        let (mut messages, mut rendered) = (Vec::new(), String::new());
        let mut r = Renderer::plain(path, src);
        for (span, msg) in errs {
            rendered.push_str(&r.render(span, &msg));
            messages.push(msg);
        }
        Failure { messages, rendered }
    }
}

/// Everything the front end has to say about a file. `Ok` means it checks clean.
fn check(path: &Path, src: &str) -> Result<(), Failure> {
    let tokens = match lexer::lex(src) {
        Ok(t) => t,
        Err(errors) => {
            let it = errors.iter().map(|e| (e.span.clone(), e.to_string()));
            return Err(Failure::of(path, src, it));
        }
    };
    let (module, errors) = parser::parse(&tokens, src.len());
    if !errors.is_empty() {
        let it = errors.iter().map(|e| (e.span.clone(), e.to_string()));
        return Err(Failure::of(path, src, it));
    }
    let Some(module) = module else {
        let it = std::iter::once((0..0, "no module".to_string()));
        return Err(Failure::of(path, src, it));
    };

    // The stdlib is declared alongside every corpus program, so `use std::io` and
    // the prelude resolve. Every corpus file is a whole program with an `fn main`.
    let std_modules = stdlib_modules();
    let mut modules: Vec<(Vec<String>, &_)> =
        std_modules.iter().map(|(p, m)| (p.clone(), m)).collect();
    modules.push((Vec::new(), &module));

    let mut env = Env::build_with(&modules, Unit::RootApplication);
    // Declarations first: a body checked against a signature that did not resolve
    // reports the same mistake twice.
    if env.errors().is_empty() {
        let (_r, errs) = neon_compiler::typecheck::check::check_module(&mut env, &module);
        env.extend_errors(errs);
    }
    if env.errors().is_empty() {
        return Ok(());
    }
    let it = env.errors().iter().map(|e| (e.span.clone(), e.to_string()));
    Err(Failure::of(path, src, it))
}

/// The parsed stdlib, read from the repo. Cheap enough to reparse per file.
fn stdlib_modules() -> Vec<(Vec<String>, neon_compiler::ast::Module)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../stdlib");
    let mut sources = Vec::new();
    collect_neon(&root, &root, &mut sources);
    neon_compiler::stdlib::parse(&sources).expect("the stdlib parses")
}

fn collect_neon(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
    for entry in std::fs::read_dir(dir).expect("stdlib is readable") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            collect_neon(root, &path, out);
        } else if path.extension().is_some_and(|e| e == "neon") {
            let rel = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            out.push((rel, std::fs::read_to_string(&path).expect("readable")));
        }
    }
}

/// The README's directive contract promises `error-contains` matches "the plain
/// text you would read on screen", so colour must never change whether a
/// directive matches. Rendering emits none today; this keeps that a rendering
/// detail rather than a thing the corpus depends on.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('\x1b') {
        out.push_str(&rest[..i]);
        match rest[i..].find('m') {
            Some(j) => rest = &rest[i + j + 1..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// `Err` is why the file does not pass, in one line.
fn outcome(path: &Path, src: &str, d: &Directives) -> Result<(), String> {
    let result = check(path, src);
    if !d.compile_fail {
        return result.map_err(|f| format!("does not check clean:\n{}", indent(&strip_ansi(&f.rendered))));
    }
    let Err(f) = result else {
        return Err("compiles clean, expected compile-fail".to_string());
    };
    let hay = strip_ansi(&f.messages.join("\n"));
    let missing: Vec<&str> =
        d.error_contains.iter().filter(|s| !hay.contains(s.as_str())).map(String::as_str).collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!("fails, but not with {missing:?}:\n{}", indent(&strip_ansi(&f.rendered))))
}

fn indent(s: &str) -> String {
    s.lines().map(|l| format!("    {l}\n")).collect()
}

fn expected_pass() -> Vec<String> {
    let path = root().join("expected-pass.txt");
    let src = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    src.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// The ratchet. See `tests/lang/README.md`, and the module comment above for
/// how much less "passing" currently means than that file implies.
#[test]
fn the_corpus_checks() {
    let files = corpus();
    let expected = expected_pass();
    let mut malformed = Vec::new();
    let mut regressed = Vec::new();
    let mut unrecorded = Vec::new();
    let mut not_yet = Vec::new();
    let mut passing = 0;

    for path in &files {
        let n = name(path);
        let Ok(src) = std::fs::read_to_string(path) else {
            malformed.push(format!("{n}: not readable as UTF-8"));
            continue;
        };
        let d = match directives(path, &src) {
            Ok(d) => d,
            Err(e) => {
                malformed.push(e);
                continue;
            }
        };
        let listed = expected.contains(&n);
        match outcome(path, &src, &d) {
            Ok(()) => {
                passing += 1;
                if !listed {
                    unrecorded.push(n);
                }
            }
            Err(why) if listed => regressed.push(format!("{n}: {why}")),
            Err(why) => not_yet.push(format!("{n}: {why}")),
        }
    }

    let stale: Vec<&String> = expected.iter().filter(|e| !files.iter().any(|p| name(p) == **e)).collect();

    // Not a failure: the README's third state. Absent and failing is "not built
    // yet", and the whole point of the ratchet is that it costs nothing to say so.
    if !not_yet.is_empty() {
        println!("{} corpus files not passing yet (not listed, not a failure):", not_yet.len());
        for f in &not_yet {
            println!("  {f}");
        }
    }

    let mut fail = String::new();
    if !malformed.is_empty() {
        fail.push_str(&format!("{} malformed corpus files:\n{}\n", malformed.len(), indent(&malformed.join("\n"))));
    }
    if !stale.is_empty() {
        fail.push_str(&format!("expected-pass.txt lists files that do not exist:\n{stale:#?}\n"));
    }
    if !regressed.is_empty() {
        fail.push_str(&format!(
            "{} files in expected-pass.txt no longer pass — a regression:\n{}\n",
            regressed.len(),
            indent(&regressed.join("\n"))
        ));
    }
    if !unrecorded.is_empty() {
        fail.push_str(&format!(
            "{} files now pass — add them to tests/lang/expected-pass.txt:\n{}\n",
            unrecorded.len(),
            indent(&unrecorded.join("\n"))
        ));
    }
    assert!(fail.is_empty(), "{passing}/{} corpus files pass\n\n{fail}", files.len());
}

