//! `neon test` has to actually run `test` blocks, and a failing assertion has to fail.
//!
//! This suite exists because a corpus test cannot catch what it is testing. The corpus
//! harness runs the *program* — its `main` — so a file whose `test` blocks are silently
//! inert passes the corpus every time. That was the bug: `test "x" { assert(1 + 1 == 3) }`
//! compiled, type-checked, and did nothing at all, because `ExprKind::Assert` had no
//! lowering and there was no verb to run a block with. Only driving the real binary and
//! reading what it prints distinguishes "the tests ran and passed" from "no test ran".
//!
//! So the load-bearing assertion in here is the negative one: a `test` block containing a
//! false assertion must make `neon test` exit non-zero and name the block. Everything else
//! guards against fixing that by failing everything.

use std::path::PathBuf;
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

/// `neon test` over `src`, in a directory of its own so parallel trials cannot collide
/// over the compiled binary.
fn neon_test(tag: &str, src: &str) -> Run {
    let dir = std::env::temp_dir().join(format!("neon_test_verb_{tag}"));
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join(format!("{tag}.neon"));
    std::fs::write(&file, src).expect("write source");

    let out = Command::new(neon())
        .env("NEON_SYSROOT", sysroot())
        .arg("test")
        .arg(&file)
        .output()
        .expect("run neon test");
    Run {
        ok: out.status.success(),
        out: String::from_utf8_lossy(&out.stdout).into_owned(),
        err: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

const ONE_PASS_ONE_FAIL: &str = "\
test \"arithmetic holds\" {
    assert(1 + 1 == 2);
}

test \"arithmetic is broken\" {
    assert(1 + 1 == 3);
}
";

/// THE REGRESSION. Before this work the program below compiled and the false assertion did
/// nothing whatsoever. A failing test must fail.
#[test]
fn a_failing_assertion_fails_the_run() {
    let r = neon_test("failing", ONE_PASS_ONE_FAIL);
    assert!(
        !r.ok,
        "a false assertion must exit non-zero\nstdout:\n{}\nstderr:\n{}",
        r.out, r.err
    );
    assert!(
        r.out.contains("arithmetic is broken") && r.out.contains("FAILED"),
        "the failing test must be named\nstdout:\n{}",
        r.out
    );
}

/// The reason `assert` is an intrinsic: it can report what was compared. "assertion failed"
/// on its own is exactly the message a stdlib function could have produced, and would mean
/// the intrinsic bought nothing.
#[test]
fn a_failure_reports_what_the_assertion_compared() {
    let r = neon_test("detail", ONE_PASS_ONE_FAIL);
    assert!(
        r.out.contains("assertion failed: 1 + 1 == 3"),
        "the failure must quote the expression that failed\nstdout:\n{}",
        r.out
    );
    assert!(
        r.out.contains("left:  2") && r.out.contains("right: 3"),
        "the failure must report both operands' values\nstdout:\n{}",
        r.out
    );
}

/// One test failing must not swallow the others: the passing block is still reported.
/// This is what the process-per-test design buys — a failed assertion panics, so an
/// in-process harness would have died before reaching the second block.
#[test]
fn a_passing_test_is_still_reported_alongside_a_failing_one() {
    let r = neon_test("both", ONE_PASS_ONE_FAIL);
    assert!(
        r.out.contains("arithmetic holds ... ok"),
        "stdout:\n{}",
        r.out
    );
    assert!(r.out.contains("1 passed; 1 failed"), "stdout:\n{}", r.out);
}

/// The guard against making everything fail. All-passing exits 0.
#[test]
fn passing_tests_exit_zero() {
    let src = "test \"holds\" {\n    assert(2 + 2 == 4);\n}\n";
    let r = neon_test("passing", src);
    assert!(r.ok, "stdout:\n{}\nstderr:\n{}", r.out, r.err);
    assert!(r.out.contains("holds ... ok"), "stdout:\n{}", r.out);
    assert!(r.out.contains("test result: ok"), "stdout:\n{}", r.out);
}

/// A file that holds only tests has no `main`, and must still compile and run. The entry
/// point of a test build is generated, so `main`'s absence is not an error there.
#[test]
fn a_file_with_no_main_still_runs_its_tests() {
    let src = "test \"no main here\" {\n    assert(1 < 2);\n}\n";
    let r = neon_test("nomain", src);
    assert!(r.ok, "stdout:\n{}\nstderr:\n{}", r.out, r.err);
    assert!(r.out.contains("no main here ... ok"), "stdout:\n{}", r.out);
}

/// `assert_eq` reports both sides too, and strings are quoted so `""` and `" "` are
/// distinguishable in the report.
#[test]
fn assert_eq_reports_both_sides() {
    let src = "test \"strings differ\" {\n    assert_eq(\"ab\", \"ac\");\n}\n";
    let r = neon_test("asserteq", src);
    assert!(!r.ok, "stdout:\n{}\nstderr:\n{}", r.out, r.err);
    assert!(
        r.out.contains("assertion failed: \"ab\" == \"ac\""),
        "stdout:\n{}",
        r.out
    );
    assert!(
        r.out.contains("left:  \"ab\"") && r.out.contains("right: \"ac\""),
        "stdout:\n{}",
        r.out
    );
}

/// `main` is compiled in a test build but never called: the entry point dispatches to a
/// test block. A `main` with side effects must not run.
#[test]
fn main_does_not_run_during_tests() {
    let src = "\
use std::io;

test \"quiet\" {
    assert(true);
}

fn main() {
    io::println(\"main must not run\");
}
";
    let r = neon_test("nomainrun", src);
    assert!(r.ok, "stdout:\n{}\nstderr:\n{}", r.out, r.err);
    assert!(!r.out.contains("main must not run"), "stdout:\n{}", r.out);
}

/// And the other direction: `neon run` on the same file runs `main` and never the tests,
/// which is what "test blocks are stripped from normal builds" means.
#[test]
fn a_normal_run_ignores_test_blocks() {
    let dir = std::env::temp_dir().join("neon_test_verb_normalrun");
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join("normalrun.neon");
    std::fs::write(
        &file,
        "use std::io;\n\ntest \"would fail\" {\n    assert(1 == 2);\n}\n\nfn main() {\n    io::println(\"main ran\");\n}\n",
    )
    .expect("write source");

    let out = Command::new(neon())
        .env("NEON_SYSROOT", sysroot())
        .arg("run")
        .arg(&file)
        .output()
        .expect("run neon run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("main ran"), "stdout:\n{stdout}");
    assert!(!stdout.contains("assertion failed"), "stdout:\n{stdout}");
}

/// A file with no `test` blocks at all is not an error.
#[test]
fn a_file_with_no_tests_says_so() {
    let src = "fn main() {}\n";
    let r = neon_test("notests", src);
    assert!(r.ok, "stdout:\n{}\nstderr:\n{}", r.out, r.err);
    assert!(r.out.contains("no tests"), "stdout:\n{}", r.out);
}
