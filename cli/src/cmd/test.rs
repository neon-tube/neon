//! `neon test` — compile a file's `test` blocks and run them.
//!
//! # What a test block compiles to
//!
//! Each `test "name" { .. }` becomes an ordinary nullary function in the IR
//! (`__neon_test_<i>`), and the C backend emits an entry point that runs *one* of them,
//! chosen by the `NEON_TEST` environment variable. This runner compiles once and then
//! spawns that binary once per test.
//!
//! The alternative — a generated `main` that walks a table of every test — was rejected
//! because a failed assertion calls `neon_panic`, which exits the process. An in-process
//! harness would report the first failure and then be gone, and the language has no way to
//! recover from a panic. One process per test is what makes "report both, name the failing
//! one" possible at all, and it contains a segfault or a corrupted heap just as well as it
//! contains an assertion.
//!
//! It also settles the `main` question for free: the entry point in a test build is
//! generated, so a file holding only tests and no `main` compiles and runs. A `main` that
//! *is* present is compiled but never called.

use crate::buildcfg::{BuildConfig, BuildFlags};
use crate::{emit, frontend};
use color_eyre::eyre::{eyre, Result};
use neon_compiler::ir::lower::test_entries;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

/// The message `neon_panic` prefixes every uncaught error with. Stripped so an assertion
/// failure reads as an assertion failure and not as a runtime error.
const PANIC_PREFIX: &str = "neon: uncaught error: ";

pub fn run(file: &OsString, filter: Option<String>, flags: BuildFlags) -> Result<()> {
    let path = PathBuf::from(file);
    let checked = frontend::check(&path, false, &[])?;
    let all = test_entries(&checked.module);

    // Filtering selects which tests to *run*, never which to compile: the indices the
    // binary dispatches on are positions in the whole module, so they stay stable.
    let selected: Vec<(usize, _)> = all
        .iter()
        .enumerate()
        .filter(|(_, t)| filter.as_ref().is_none_or(|f| t.name.contains(f.as_str())))
        .collect();

    if all.is_empty() {
        println!("no tests");
        return Ok(());
    }

    let cfg = BuildConfig::resolve(&path, flags)?;
    let dir = std::env::temp_dir().join("neon-test");
    std::fs::create_dir_all(&dir)?;
    let stem = path.file_stem().unwrap_or_else(|| "program".as_ref());
    let exe = emit::executable_path(dir.join(stem));
    emit::to_test_executable(&checked, &all, &exe, &cfg)?;

    println!(
        "running {} test{}\n",
        selected.len(),
        if selected.len() == 1 { "" } else { "s" }
    );
    let mut failed = 0usize;
    for (index, entry) in &selected {
        match run_one(&exe, *index)? {
            None => println!("test {} ... ok", entry.name),
            Some(detail) => {
                failed += 1;
                println!("test {} ... FAILED", entry.name);
                for line in detail.lines() {
                    println!("    {line}");
                }
            }
        }
    }

    let passed = selected.len() - failed;
    println!();
    if failed == 0 {
        println!("test result: ok. {passed} passed; 0 failed");
        Ok(())
    } else {
        println!("test result: FAILED. {passed} passed; {failed} failed");
        std::process::exit(1);
    }
}

/// Run one test in its own process. `None` is a pass; `Some(detail)` is a failure, with
/// whatever the test wrote and whatever killed it.
///
/// A clean exit 0 is the only pass. Everything else — a failed assertion's `neon_panic`, a
/// trap, a signal — is a failure, which is the right default: a test that died is not a
/// test that passed, whatever it died of.
fn run_one(exe: &std::path::Path, index: usize) -> Result<Option<String>> {
    let out = Command::new(exe)
        .env("NEON_TEST", index.to_string())
        .output()
        .map_err(|e| eyre!("could not run {}: {e}", exe.display()))?;
    if out.status.success() {
        return Ok(None);
    }

    let mut detail = String::new();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stderr = stderr.trim_end();
    if stderr.is_empty() {
        // Killed without saying anything — a signal, or a trap that never got to print.
        detail.push_str(&match out.status.code() {
            Some(c) => format!("the test exited with status {c}"),
            None => "the test was killed by a signal".to_string(),
        });
    } else {
        detail.push_str(stderr.strip_prefix(PANIC_PREFIX).unwrap_or(stderr));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !stdout.trim().is_empty() {
        detail.push_str("\n--- stdout ---\n");
        detail.push_str(stdout.trim_end());
    }
    Ok(Some(detail))
}
