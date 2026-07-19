//! The backend's end-to-end oracle: compile every passing corpus program that has a
//! `.stdout`, run it, and diff. This is what the README's execution contract always
//! meant; it lights up as the C backend widens.
//!
//! Each corpus program is registered as its own libtest-mimic trial, so `cargo nextest
//! run -p neon-compiler --test backend_run` runs and reports them individually and in
//! parallel across processes — no hand-rolled thread pool, and a failure names the one
//! program that regressed rather than a lumped-together count.

use libtest_mimic::{Arguments, Failed, Trial};
use neon_compiler::backend::c;
use neon_compiler::ir::{self, Stage};
use neon_compiler::typecheck::env::Unit;
use neon_compiler::typecheck::{check::check_all, Env};
use neon_compiler::{lexer, parser};
use std::path::{Path, PathBuf};
use std::process::Command;

fn lang_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/lang")
}
fn runtime_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../runtime")
}

/// The stdlib, numbered from 0, plus the next free expression id. The program is numbered
/// after it so ids are unique across the whole compilation — the stdlib's bodies are real
/// Neon code that gets checked and lowered alongside the program.
fn stdlib_modules() -> (Vec<(Vec<String>, neon_compiler::ast::Module)>, u32) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../stdlib");
    let mut sources = Vec::new();
    collect_neon(&root, &root, &mut sources);
    neon_compiler::stdlib::parse_from(&sources, 0).expect("stdlib parses")
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

/// The C source for a corpus file, or an error string if it does not check clean.
fn emit_c(src: &str) -> Result<String, String> {
    let tokens = lexer::lex(src).map_err(|_| "lex error".to_string())?;
    let (module, perrs) = parser::parse(&tokens, src.len());
    if !perrs.is_empty() {
        return Err("parse error".into());
    }
    let mut module = module.ok_or("no module")?;
    let (std_owned, next_id) = stdlib_modules();
    neon_compiler::ast::number_exprs_from(&mut module, next_id);

    let mut modules: Vec<(Vec<String>, &_)> = std_owned.iter().map(|(p, m)| (p.clone(), m)).collect();
    modules.push((Vec::new(), &module));
    let mut env = Env::build_with(&modules, Unit::RootApplication);
    if !env.errors().is_empty() {
        return Err("declaration errors".into());
    }
    let (result, errs) = check_all(&mut env, &modules);
    if !errs.is_empty() {
        return Err(format!("type errors: {errs:?}"));
    }
    let libs: Vec<(Vec<String>, &_)> = std_owned.iter().map(|(p, m)| (p.clone(), m)).collect();
    Ok(c::emit(&ir::compile(&env, &result, &module, &libs, Stage::Final)))
}

/// Compile a corpus program to C, build it with `cc`, run it, and diff against `.stdout`.
fn run_one(dir: &Path, rel: &str) -> Result<(), Failed> {
    let src = std::fs::read_to_string(lang_root().join(rel)).expect("readable");
    let c = emit_c(&src).map_err(|e| Failed::from(format!("emit: {e}")))?;

    let stem = rel.replace(['/', '.'], "_");
    let c_file = dir.join(format!("{stem}.c"));
    let exe = dir.join(&stem);
    std::fs::write(&c_file, &c).expect("write c");

    let out = Command::new(std::env::var("CC").unwrap_or_else(|_| "cc".into()))
        // Sanitizers are on by default, not an opt-in sweep. A corpus that passes without
        // them proves only that the answers look right: this suite ran green over a genuine
        // stack-buffer-overflow, where a value handed to `neon_map_set` uncoerced had the
        // witness memcpy 32 bytes out of an 8-byte double.
        .args([
            "-std=c11",
            "-w",
            "-O0",
            "-g",
            "-fno-omit-frame-pointer",
            "-fsanitize=address,undefined",
            "-o",
        ])
        .arg(&exe)
        .arg(&c_file)
        .arg(runtime_root().join("src/rt.c"))
        .arg("-I")
        .arg(runtime_root().join("include"))
        // `std::math` bottoms out in libm, separate from libc on Linux.
        .arg("-lm")
        .output()
        .expect("run cc");
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr).lines().take(4).collect::<Vec<_>>().join("\n");
        return Err(Failed::from(format!("cc failed:\n{msg}")));
    }

    // Bound the run: a backend bug can emit a program that never terminates, and a hung
    // child would otherwise stall the whole suite. `timeout` SIGKILLs after 10s.
    let run = Command::new("timeout")
        .args(["-k", "1", "30"])
        .arg(&exe)
        .env("ASAN_OPTIONS", "detect_leaks=1:abort_on_error=0")
        .env("UBSAN_OPTIONS", "print_stacktrace=1")
        .output()
        .expect("run exe");
    let code = run.status.code();
    if code == Some(124) || code == Some(137) {
        return Err(Failed::from("timed out"));
    }

    // A sanitizer report is a failure on its own terms, ahead of any output diff: the
    // program may well print the right answer while corrupting memory to do it.
    let stderr = String::from_utf8_lossy(&run.stderr).to_string();
    if let Some(report) = sanitizer_report(&stderr) {
        return Err(Failed::from(format!("sanitizer:\n{report}")));
    }

    // A program that traps prints to stdout up to the fault, then exits with a code the
    // `//@ exit:` annotation pins (0 when absent). Both stdout and the code must match.
    let want_exit = expected_exit(&src);
    let got = String::from_utf8_lossy(&run.stdout).to_string();
    let want = std::fs::read_to_string(lang_root().join(rel).with_extension("stdout")).unwrap_or_default();
    if got != want {
        return Err(Failed::from(format!("output mismatch\n  got:  {got:?}\n  want: {want:?}")));
    }
    if code != Some(want_exit) {
        let err = String::from_utf8_lossy(&run.stderr).trim().to_string();
        return Err(Failed::from(format!("exit {code:?}, want {want_exit}\n  stderr: {err}")));
    }
    Ok(())
}

/// The first few lines of a sanitizer report, if the run produced one.
fn sanitizer_report(stderr: &str) -> Option<String> {
    let marks = ["AddressSanitizer", "LeakSanitizer", "runtime error:", "SUMMARY:"];
    stderr.lines().any(|l| marks.iter().any(|m| l.contains(m))).then(|| {
        stderr
            .lines()
            .filter(|l| !l.trim().is_empty())
            .take(6)
            .collect::<Vec<_>>()
            .join("\n")
    })
}

/// The exit code a program is expected to end with, from a `//@ exit: N` directive; 0 when
/// there is none.
fn expected_exit(src: &str) -> i32 {
    src.lines()
        .map_while(|l| {
            let t = l.trim_start();
            (t.starts_with("//") || t.is_empty()).then_some(t)
        })
        .find_map(|l| l.strip_prefix("//@ exit:").and_then(|n| n.trim().parse().ok()))
        .unwrap_or(0)
}

fn main() {
    let args = Arguments::from_args();

    let dir = std::env::temp_dir().join("neon_backend_run");
    std::fs::create_dir_all(&dir).expect("temp dir");

    // One trial per corpus program that has an expected `.stdout`.
    let trials: Vec<Trial> = expected_pass()
        .into_iter()
        .filter(|rel| lang_root().join(rel).with_extension("stdout").is_file())
        .map(|rel| {
            let name = rel.strip_suffix(".neon").unwrap_or(&rel).to_string();
            let dir = dir.clone();
            Trial::test(name, move || run_one(&dir, &rel))
        })
        .collect();

    libtest_mimic::run(&args, trials).exit();
}
