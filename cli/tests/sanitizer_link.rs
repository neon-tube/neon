//! Every accepted `--sanitize` spelling must produce a binary that links **and runs**.
//!
//! This suite exists because a unit test of the selection function was not enough. The
//! selection function was right — it named `libneon_rt_san.a` for every accepted spelling
//! — and the build was still broken for two of the four, because the bug was downstream of
//! the choice: the archive is compiled `-fsanitize=address,undefined`, so its objects call
//! into both sanitizer runtimes, and a link that enabled only the subset the user typed
//! left the other's symbols undefined:
//!
//!     --sanitize address    -> undefined reference to `__ubsan_handle_type_mismatch_v1'
//!     --sanitize undefined  -> undefined reference to `__asan_report_load4'
//!
//! `backend_run` could not catch it: it uses address+undefined together, which was the one
//! combination that worked. Only driving the real CLI through to a real linker does, so
//! that is what these tests do.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The staged sysroot lives beside the CLI binary (`cli/build.rs` puts it there).
fn neon() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_neon"))
}
fn sysroot() -> PathBuf {
    neon().parent().expect("binary has a parent").to_path_buf()
}

const PROGRAM: &str = "use std::io;\n\nfn main() {\n    io::println(\"sanitized\");\n}\n";

/// Compile `PROGRAM` with `args`, in a directory unique to `tag`.
fn compile(tag: &str, args: &[&str]) -> (std::process::Output, PathBuf) {
    let dir = std::env::temp_dir().join(format!("neon_sanitizer_link/{tag}"));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let src = dir.join("prog.neon");
    std::fs::write(&src, PROGRAM).expect("write source");
    let exe = dir.join("prog");
    let _ = std::fs::remove_file(&exe);

    let out = Command::new(neon())
        .arg("compile")
        .arg(&src)
        .arg("-o")
        .arg(&exe)
        .args(args)
        .env("NEON_SYSROOT", sysroot())
        .output()
        .expect("run neon");
    (out, exe)
}

fn assert_links_and_runs(tag: &str, args: &[&str]) -> String {
    let (out, exe) = compile(tag, args);
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        out.status.success(),
        "`neon compile {}` failed:\n{stderr}",
        args.join(" ")
    );
    // Name the symptom directly: a linker dump here is the exact regression this file
    // guards, and the generic "compile failed" above would bury it.
    assert!(
        !stderr.contains("undefined reference"),
        "`neon compile {}` produced undefined references:\n{stderr}",
        args.join(" ")
    );
    assert!(exe.is_file(), "no executable at {}", exe.display());

    let run = Command::new(&exe)
        // A leak report would fail the run for reasons unrelated to linking; this suite
        // is about whether the sanitized runtime links and executes.
        .env("ASAN_OPTIONS", "detect_leaks=0")
        .output()
        .expect("run program");
    assert!(
        run.status.success(),
        "the {} binary did not run cleanly:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&run.stdout), "sanitized\n");
    stderr
}

/// Both sanitizers, in both spellings: the combination that always worked.
#[test]
fn full_set_links_and_runs() {
    for (tag, args) in [
        ("both_comma", &["--sanitize", "address,undefined"][..]),
        (
            "both_repeated",
            &["--sanitize", "address", "--sanitize", "undefined"][..],
        ),
    ] {
        let stderr = assert_links_and_runs(tag, args);
        // Nothing was widened, so nothing should be announced.
        assert!(
            !stderr.contains("also enabling"),
            "unexpected widening note:\n{stderr}"
        );
    }
}

/// The regression: each sanitizer alone. These are the two spellings that failed to link.
#[test]
fn each_sanitizer_alone_links_and_runs_and_says_it_widened() {
    for (tag, one, other) in [
        ("address_only", "address", "undefined"),
        ("undefined_only", "undefined", "address"),
    ] {
        let stderr = assert_links_and_runs(tag, &["--sanitize", one]);
        // Widening to the archive's full set is safe but must not be silent.
        assert!(
            stderr.contains("also enabling") && stderr.contains(other),
            "asking for `{one}` alone must report that `{other}` was added too:\n{stderr}"
        );
    }
}

/// A sanitizer with no instrumented archive is refused with the CLI's own error, before
/// any compiler runs — not left to surface as a linker dump.
#[test]
fn an_unsupported_sanitizer_is_refused_with_our_error() {
    let (out, exe) = compile("thread", &["--sanitize", "thread"]);
    assert!(
        !out.status.success(),
        "`--sanitize thread` must not succeed"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("thread"),
        "the error must name the sanitizer:\n{stderr}"
    );
    assert!(
        stderr.contains("libneon_rt_san.a"),
        "the error must say which variants exist:\n{stderr}"
    );
    assert!(
        !stderr.contains("undefined reference") && !stderr.contains("/usr/bin/ld"),
        "must fail with our error, not a linker dump:\n{stderr}"
    );
    assert!(
        !exe.is_file(),
        "a refused build must not leave an executable"
    );
}

/// The unsanitized modes still link their own archives.
#[test]
fn unsanitized_modes_link_and_run() {
    for mode in ["debug", "release", "opt-release"] {
        let stderr = assert_links_and_runs(mode, &["--mode", mode]);
        assert!(
            !stderr.contains("also enabling"),
            "no sanitizers, no widening:\n{stderr}"
        );
    }
}

/// Guards the assumption the whole suite rests on: the sysroot beside the CLI really does
/// carry all three variants. Without this a staging regression would look like a
/// sanitizer bug.
#[test]
fn every_variant_is_staged() {
    // At least one flavor must be complete; a flavor whose compiler is absent from the
    // build machine is legitimately unstaged. The suite's own links go through the CLI,
    // which picks the flavor matching its `cc`.
    let staged: Vec<String> = ["gcc", "clang"]
        .iter()
        .filter(|f| sysroot().join("lib").join(f).join("libneon_rt.a").is_file())
        .map(|f| f.to_string())
        .collect();
    assert!(!staged.is_empty(), "no runtime flavor staged under lib/");
    for flavor in &staged {
        for archive in ["libneon_rt.a", "libneon_rt_debug.a", "libneon_rt_san.a"] {
            let path: PathBuf = sysroot().join("lib").join(flavor).join(archive);
            assert!(
                path.is_file(),
                "missing staged runtime variant {}",
                path.display()
            );
        }
    }
    assert!(
        Path::new(&sysroot().join("stdlib")).is_dir(),
        "stdlib not staged"
    );
}
