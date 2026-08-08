//! `neon doctor`: diagnose the whole setup, then prove it with an end-to-end build.
//!
//! Every check prints a line whether it passes or not — a doctor that only speaks when
//! something is wrong cannot be told apart from one that did not look. The verdict at
//! the bottom is the exit code: hard failures (no sysroot, a smoke test that does not
//! run) exit non-zero; warnings (a missing flavor, a compiler without archives) do not,
//! because the setup still builds — the warnings say what it costs.

use crate::buildcfg::{BuildConfig, BuildFlags, RuntimeVariant};
use crate::sysroot::Sysroot;
use color_eyre::eyre::{eyre, Result};
use std::path::Path;
use std::process::Command;

struct Report {
    warnings: usize,
    failures: usize,
}

impl Report {
    fn ok(&mut self, msg: &str) {
        println!("  [ ok ] {msg}");
    }
    fn warn(&mut self, msg: &str) {
        self.warnings += 1;
        println!("  [warn] {msg}");
    }
    fn fail(&mut self, msg: &str) {
        self.failures += 1;
        println!("  [FAIL] {msg}");
    }
}

pub fn run() -> Result<()> {
    let mut r = Report {
        warnings: 0,
        failures: 0,
    };

    println!("toolchain");
    let via = if std::env::var_os("NEON_SYSROOT").is_some() {
        "via NEON_SYSROOT"
    } else {
        "beside the binary"
    };
    let sysroot = match Sysroot::find() {
        Ok(s) => {
            r.ok(&format!("sysroot: {} ({via})", s.root().display()));
            Some(s)
        }
        Err(e) => {
            r.fail(&format!("sysroot: {e}"));
            None
        }
    };

    if let Some(s) = &sysroot {
        if s.include().join("libneon_rt.h").is_file() {
            r.ok(&format!(
                "include: {} (umbrella header present)",
                s.include().display()
            ));
        } else {
            r.fail(&format!(
                "include: {} has no libneon_rt.h — emitted C cannot compile",
                s.include().display()
            ));
        }
        let stdlib = s.stdlib();
        let count = count_neon_files(&stdlib);
        if count > 0 {
            r.ok(&format!(
                "stdlib:  {} ({count} source files)",
                stdlib.display()
            ));
        } else {
            r.fail(&format!(
                "stdlib:  {} is missing or empty",
                stdlib.display()
            ));
        }

        println!("runtime archives");
        let variants = [
            RuntimeVariant::Release,
            RuntimeVariant::Debug,
            RuntimeVariant::Sanitized,
        ];
        for flavor in ["gcc", "clang"] {
            let dir = s.lib_dir().join(flavor);
            let missing: Vec<&str> = variants
                .iter()
                .map(|v| v.archive())
                .filter(|a| !dir.join(a).is_file())
                .collect();
            if missing.is_empty() {
                r.ok(&format!("{flavor}/: all three variants present"));
            } else if missing.len() == variants.len() {
                // `Sysroot::runtime_lib` stands the other flavor's archive in only when it
                // carries machine code: a bitcode-only one (plain `-flto`, which is the
                // macOS shape) it refuses, because the other family's linker cannot read a
                // bitcode member at all. Report whichever of the two this toolchain is —
                // "loses LTO" and "cannot build" are very different things to plan around.
                let other = if flavor == "gcc" { "clang" } else { "gcc" };
                let stand_in = s
                    .lib_dir()
                    .join(other)
                    .join(RuntimeVariant::Release.archive());
                let linkable =
                    crate::sysroot::inspect_archive(&stand_in).is_none_or(|c| c.native_code);
                r.warn(&format!(
                    "{flavor}/: no archives — {}",
                    if linkable {
                        format!(
                            "a {flavor} `cc` falls back to the {other} release/debug archive \
                             (losing LTO) and cannot do sanitized builds"
                        )
                    } else {
                        format!(
                            "the {other} archives are LTO bitcode with no machine code in \
                             them, so a {flavor} `cc` cannot link here at all. Build with \
                             {other} (`--cc`/`$CC`), or rebuild the toolchain on a machine \
                             with {flavor} installed"
                        )
                    }
                ));
            } else {
                r.fail(&format!(
                    "{flavor}/: incomplete — missing {} (a partial flavor is a broken \
                     staging, not a build-machine limitation)",
                    missing.join(", ")
                ));
            }
        }
    }

    println!("compilers");
    // Resolve exactly as a build would from here, so a project neon.toml's `cc` and
    // `$CC` are both honored and diagnosed.
    let cfg = BuildConfig::resolve(Path::new("."), BuildFlags::default());
    let mut resolved_cc: Option<String> = None;
    match &cfg {
        Ok(cfg) => match cfg.cc_flavor() {
            Ok(flavor) => {
                let version = version_line(&cfg.cc).unwrap_or_else(|| "version unknown".into());
                resolved_cc = Some(cfg.cc.clone());
                let has_archives = sysroot
                    .as_ref()
                    .is_some_and(|s| s.flavors_present().contains(&flavor.dir()));
                if has_archives {
                    r.ok(&format!(
                        "cc = `{}` -> {} ({version}); archives present",
                        cfg.cc,
                        flavor.dir()
                    ));
                } else {
                    r.warn(&format!(
                        "cc = `{}` -> {} ({version}); no {} archives staged — release \
                         builds fall back with a warning, sanitized builds refuse",
                        cfg.cc,
                        flavor.dir(),
                        flavor.dir()
                    ));
                }
            }
            Err(e) => r.fail(&format!("cc = `{}` cannot be identified: {e}", cfg.cc)),
        },
        Err(e) => r.fail(&format!("build configuration does not resolve: {e}")),
    }
    for probe in ["gcc", "clang"] {
        match version_line(probe) {
            Some(v) => r.ok(&format!("{probe} on PATH: {v}")),
            None => r.warn(&format!(
                "{probe} not on PATH — toolchain rebuilds will not stage {probe} archives"
            )),
        }
    }

    println!("smoke test");
    if r.failures == 0 {
        if let Some(cc) = resolved_cc {
            smoke(&mut r, &cc, &[]);
            smoke(&mut r, &cc, &["--sanitize", "address,undefined"]);
        }
    } else {
        println!("  [skip] not attempted: the checks above already failed");
    }

    println!();
    match (r.failures, r.warnings) {
        (0, 0) => {
            println!("verdict: healthy");
            Ok(())
        }
        (0, w) => {
            println!(
                "verdict: working, {w} warning{}",
                if w == 1 { "" } else { "s" }
            );
            Ok(())
        }
        (f, _) => {
            println!(
                "verdict: broken, {f} failure{}",
                if f == 1 { "" } else { "s" }
            );
            Err(eyre!(
                "`neon doctor` found {f} failure{}",
                if f == 1 { "" } else { "s" }
            ))
        }
    }
}

/// Compile and run a one-line program through the real pipeline — self-exec, so the
/// child resolves the sysroot exactly as any user build would. Run from a temp dir so a
/// surrounding project's `neon.toml` cannot leak into what was diagnosed above; the
/// resolved `cc` is passed explicitly for the same reason.
fn smoke(r: &mut Report, cc: &str, extra: &[&str]) {
    let label = if extra.is_empty() {
        "release"
    } else {
        "sanitized"
    };
    let dir = std::env::temp_dir().join(format!("neon-doctor-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("main.neon");
    let bin = dir.join("main");
    let program = "use std::io;\n\nfn main() {\n    io::println(\"doctor ok\");\n}\n";
    if let Err(e) = std::fs::write(&src, program) {
        r.fail(&format!("{label}: cannot write {}: {e}", src.display()));
        return;
    }
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            r.fail(&format!("{label}: cannot locate the neon binary: {e}"));
            return;
        }
    };
    let compile = Command::new(&exe)
        .arg("compile")
        .arg(&src)
        .arg("-o")
        .arg(&bin)
        .arg("--cc")
        .arg(cc)
        .args(extra)
        .current_dir(&dir)
        .output();
    match compile {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            r.fail(&format!(
                "{label}: compile failed:\n{}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
            return;
        }
        Err(e) => {
            r.fail(&format!("{label}: cannot run the compiler: {e}"));
            return;
        }
    }
    match Command::new(&bin).output() {
        Ok(out) if out.status.success() && out.stdout == b"doctor ok\n" => {
            r.ok(&format!(
                "{label}: compiled, linked, ran, and printed what it should"
            ));
        }
        Ok(out) => r.fail(&format!(
            "{label}: the compiled program misbehaved (exit {:?}, stdout {:?})",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout)
        )),
        Err(e) => r.fail(&format!("{label}: compiled but did not run: {e}")),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

fn version_line(cc: &str) -> Option<String> {
    let out = Command::new(cc).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(str::to_string)
}

fn count_neon_files(dir: &Path) -> usize {
    let mut n = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            n += count_neon_files(&p);
        } else if p.extension().is_some_and(|x| x == "neon") {
            n += 1;
        }
    }
    n
}
