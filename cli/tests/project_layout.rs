//! Multi-file projects: every `src/**/*.neon` beside `main.neon` is a module named by
//! its path — `src/util.neon` is `util`, `src/net/http.neon` is `net::http` — the same
//! rule the stdlib's files follow.
//!
//! This suite drives the real binary because the corpus cannot: corpus files are single
//! programs, and what multi-file adds is precisely the part outside any one file — the
//! walk, cross-module resolution, diagnostics landing on the right file, and `neon test`
//! finding a block that lives in a module rather than the entry.

use std::path::{Path, PathBuf};
use std::process::Command;

fn neon() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_neon"))
}

/// The staged sysroot lives beside the CLI binary; `cli/build.rs` puts it there.
fn sysroot() -> PathBuf {
    neon().parent().expect("binary has a parent").to_path_buf()
}

struct Run {
    ok: bool,
    out: String,
    err: String,
}

fn neon_in(dir: &Path, args: &[&str]) -> Run {
    let out = Command::new(neon())
        .env("NEON_SYSROOT", sysroot())
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run neon");
    Run {
        ok: out.status.success(),
        out: String::from_utf8_lossy(&out.stdout).into_owned(),
        err: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// A fresh project directory under the OS temp dir, tagged so parallel trials cannot
/// collide, with `files` written relative to its root.
fn project(tag: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("neon_project_layout_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).expect("mkdir");
    std::fs::write(dir.join("neon.toml"), "[package]\nname = \"app\"\n").expect("manifest");
    for (rel, text) in files {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdirs");
        std::fs::write(&p, text).expect("write");
    }
    dir
}

#[test]
fn modules_resolve_across_files_and_the_program_runs() {
    let dir = project(
        "runs",
        &[
            (
                "src/main.neon",
                "use std::io;\nuse util;\nuse net::http;\n\nfn main() {\n    io::println(util::greeting(\"world\"));\n    io::println(http::host(\"neon.dev/docs\"));\n}\n",
            ),
            (
                "src/util.neon",
                "fn greeting(who: str) -> str { \"hello, #{who}\" }\n",
            ),
            (
                "src/net/http.neon",
                "use std::string;\n\nfn host(url: str) -> str {\n    let at = string::find(url, \"/\");\n    if at is i64 {\n        try? string::slice(url, 0, at) orelse url\n    } else {\n        url\n    }\n}\n",
            ),
        ],
    );
    let build = neon_in(&dir, &["build"]);
    assert!(build.ok, "build failed:\n{}{}", build.out, build.err);

    let exe = dir.join("_neon").join("app");
    let out = Command::new(&exe).output().expect("run built app");
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "hello, world\nneon.dev\n"
    );
}

#[test]
fn a_type_error_in_a_module_names_that_file() {
    let dir = project(
        "diag",
        &[
            (
                "src/main.neon",
                "use util;\nfn main() { util::answer(); }\n",
            ),
            ("src/util.neon", "fn answer() -> i64 { \"not a number\" }\n"),
        ],
    );
    let build = neon_in(&dir, &["build"]);
    assert!(!build.ok, "a type error must fail the build");
    assert!(
        build.err.contains("src/util.neon"),
        "the diagnostic must land on the module's file, not the entry:\n{}",
        build.err
    );
}

#[test]
fn neon_test_runs_a_block_that_lives_in_a_module() {
    let dir = project(
        "tests",
        &[
            ("src/main.neon", "use util;\nfn main() {}\n"),
            (
                "src/util.neon",
                "fn double(n: i64) -> i64 { n * 2 }\n\ntest \"double doubles\" {\n    assert(double(21) == 42);\n}\n\ntest \"double is broken\" {\n    assert(double(1) == 3);\n}\n",
            ),
        ],
    );
    let run = neon_in(&dir, &["test"]);
    assert!(!run.ok, "a failing test must fail the verb:\n{}", run.out);
    assert!(
        run.out.contains("test double doubles ... ok"),
        "{}",
        run.out
    );
    assert!(
        run.out.contains("test double is broken ... FAILED"),
        "{}",
        run.out
    );
}

#[test]
fn neon_check_covers_the_whole_project() {
    let dir = project(
        "check",
        &[
            ("src/main.neon", "fn main() {}\n"),
            // Broken, and unreferenced by the entry: only a check that walks every
            // module can see it.
            ("src/dusty.neon", "fn bad() -> i64 { \"nope\" }\n"),
        ],
    );
    let run = neon_in(&dir, &["check"]);
    assert!(!run.ok, "check must fail on a broken module:\n{}", run.err);
    assert!(run.err.contains("src/dusty.neon"), "{}", run.err);
}
