//! `neon bench` — compile `bench` blocks and time them.
//!
//! The shape is `neon test`'s: each block becomes a nullary function, the binary times
//! ONE of them (chosen by `NEON_BENCH`), and this runner compiles once then spawns per
//! bench — so a bench that traps kills only itself, and each starts from a cold runtime
//! rather than inheriting the previous block's heap.
//!
//! The measurement is the binary's (see `emit_bench_entry`): warm up once, double the
//! batch until it runs at least 200ms, print ns/iteration. This runner only formats.
//! What that measures honestly: whole-body iteration cost under real codegen. What it
//! cannot promise: that a body computing an unused pure value is not partly optimised
//! away — the function-pointer dispatch stops the C compiler from deleting the *loop*,
//! but a body's own dead code is its own business. Benches whose result matters should
//! use it (print it, index with it).

use crate::buildcfg::{BuildConfig, BuildFlags};
use crate::{emit, frontend};
use color_eyre::eyre::{eyre, Result};
use neon_compiler::ir::lower::bench_entries_with;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

pub fn run(file: Option<&OsString>, filter: Option<String>, flags: BuildFlags) -> Result<()> {
    // A named file runs alone; no file means the whole project, benches in modules
    // included — the same rule as `neon test`.
    let (path, checked) = match file {
        Some(f) => {
            let path = PathBuf::from(f);
            let checked = frontend::check(&path, false, &[])?;
            (path, checked)
        }
        None => {
            let project = crate::project::Project::find(std::path::Path::new("."))?;
            let modules = project.modules()?;
            let checked = frontend::check_project(&project.entry(), &modules, false, &[])?;
            (project.entry(), checked)
        }
    };
    let libs: Vec<(Vec<String>, &_)> = checked.libs.iter().map(|(p, m)| (p.clone(), m)).collect();
    let all = bench_entries_with(&checked.module, &libs);

    let selected: Vec<(usize, _)> = all
        .iter()
        .enumerate()
        .filter(|(_, b)| filter.as_ref().is_none_or(|f| b.name.contains(f.as_str())))
        .collect();

    if all.is_empty() {
        println!("no benches");
        return Ok(());
    }

    let cfg = BuildConfig::resolve(&path, flags)?;
    let dir = std::env::temp_dir().join("neon-bench");
    std::fs::create_dir_all(&dir)?;
    let stem = path.file_stem().unwrap_or_else(|| "program".as_ref());
    let exe = emit::executable_path(dir.join(stem));
    emit::to_bench_executable(&checked, &all, &exe, &cfg)?;

    println!(
        "running {} bench{}\n",
        selected.len(),
        if selected.len() == 1 { "" } else { "es" }
    );
    let mut failed = 0usize;
    for (index, entry) in &selected {
        match run_one(&exe, *index)? {
            Ok(ns) => println!("bench {} ... {}", entry.name, human(ns)),
            Err(detail) => {
                failed += 1;
                println!("bench {} ... FAILED", entry.name);
                for line in detail.lines() {
                    println!("    {line}");
                }
            }
        }
    }

    if failed == 0 {
        Ok(())
    } else {
        println!();
        println!(
            "{failed} bench{} failed",
            if failed == 1 { "" } else { "es" }
        );
        std::process::exit(1);
    }
}

/// ns/iteration scaled to the unit a human reads at a glance. One decimal, because
/// run-to-run noise makes more digits a lie.
fn human(ns: f64) -> String {
    if ns < 1_000.0 {
        format!("{ns:.1} ns/iter")
    } else if ns < 1_000_000.0 {
        format!("{:.1} µs/iter", ns / 1_000.0)
    } else if ns < 1_000_000_000.0 {
        format!("{:.1} ms/iter", ns / 1_000_000.0)
    } else {
        format!("{:.2} s/iter", ns / 1_000_000_000.0)
    }
}

/// Run one bench in its own process: `Ok(Ok(ns))` is a measurement, `Ok(Err(detail))`
/// is a bench that died — trapped, panicked, or printed something unparseable.
fn run_one(exe: &std::path::Path, index: usize) -> Result<std::result::Result<f64, String>> {
    let out = Command::new(exe)
        .env("NEON_BENCH", index.to_string())
        .output()
        .map_err(|e| eyre!("could not run {}: {e}", exe.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if out.status.success() {
        // The measurement is the LAST line: the body may print on its own, and that
        // output belongs to it, not to the protocol.
        if let Some(ns) = stdout
            .lines()
            .next_back()
            .and_then(|l| l.trim().parse::<f64>().ok())
        {
            return Ok(Ok(ns));
        }
    }
    let mut detail = String::new();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stderr = stderr.trim_end();
    if stderr.is_empty() {
        detail.push_str(&match out.status.code() {
            Some(c) => format!("the bench exited with status {c}"),
            None => "the bench was killed by a signal".to_string(),
        });
    } else {
        detail.push_str(stderr);
    }
    Ok(Err(detail))
}
