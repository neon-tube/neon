//! Every diagnostic the front end produces has to reach the user.
//!
//! This suite exists because a corpus test could not catch what it is testing. Checking
//! raises errors through two channels — the list `check_all` returns, and `Env::errors`,
//! where resolving a type annotation raises. The corpus harness always read both. The CLI
//! read `Env::errors` only *before* checking, so an unknown type written inside a function
//! body was resolved during checking, its error landed in the channel nobody re-read, and
//! the program compiled with a poison type in it. The backend's guard against an
//! unsubstituted type variable then fired, and the user saw an internal compiler error
//! instead of the diagnostic the compiler had already produced and discarded.
//!
//! A corpus file pinning the same program passes either way, because the harness reads
//! both lists. Only driving the real binary and reading its stderr distinguishes them, so
//! that is what these tests do.

use std::path::PathBuf;
use std::process::Command;

fn neon() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_neon"))
}

/// The staged sysroot lives beside the CLI binary; `cli/build.rs` puts it there.
fn sysroot() -> PathBuf {
    neon().parent().expect("binary has a parent").to_path_buf()
}

/// Run a verb over `src` in a directory of its own, returning (exit ok, stderr).
fn run(tag: &str, verb: &str, src: &str) -> (bool, String) {
    let dir = std::env::temp_dir().join(format!("neon_diag_{tag}"));
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join("main.neon");
    std::fs::write(&file, src).expect("write source");

    let out = Command::new(neon())
        .env("NEON_SYSROOT", sysroot())
        .arg(verb)
        .arg(&file)
        .args(if verb == "compile" {
            vec!["-o".into(), dir.join("out")]
        } else {
            vec![]
        })
        .output()
        .expect("run neon");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

const UNKNOWN_IN_BODY: &str = "\
use std::io;

fn main() {
    let x: CompletelyMadeUpType = 5;
    io::println(\"must not compile\");
}
";

/// The regression. `compile` must refuse, and must say *why* — an internal error naming a
/// type variable is not a diagnostic, it is the absence of one.
#[test]
fn compile_reports_an_unknown_type_written_in_a_body() {
    let (ok, err) = run("compile_body", "compile", UNKNOWN_IN_BODY);
    assert!(!ok, "compiling an unknown type must fail; stderr:\n{err}");
    assert!(
        err.contains("unknown type") && err.contains("CompletelyMadeUpType"),
        "the diagnostic must name the type; stderr:\n{err}"
    );
    assert!(
        !err.contains("internal error"),
        "a user's bad type name must not surface as an internal compiler error; stderr:\n{err}"
    );
}

/// `check` always got this right, and must keep doing so — the two verbs share
/// `frontend::check`, so this pins that they agree rather than diverge again.
#[test]
fn check_reports_an_unknown_type_written_in_a_body() {
    let (ok, err) = run("check_body", "check", UNKNOWN_IN_BODY);
    assert!(!ok, "checking an unknown type must fail; stderr:\n{err}");
    assert!(err.contains("unknown type"), "stderr:\n{err}");
}

/// The other channel, which already worked: an annotation on a *declaration* resolves
/// during `Env::build_with`, before checking. Kept so a fix to one channel cannot quietly
/// break the other.
#[test]
fn compile_reports_an_unknown_type_in_a_signature() {
    let src = "fn takes(x: AlsoMadeUp) -> i64 { 1 }\n\nfn main() {}\n";
    let (ok, err) = run("compile_sig", "compile", src);
    assert!(!ok, "stderr:\n{err}");
    assert!(
        err.contains("unknown type") && err.contains("AlsoMadeUp"),
        "stderr:\n{err}"
    );
}

/// And a valid program still compiles — the guard against fixing this by rejecting
/// everything.
#[test]
fn a_valid_program_still_compiles() {
    let src = "use std::io;\n\nfn main() {\n    io::println(\"fine\");\n}\n";
    let (ok, err) = run("valid", "compile", src);
    assert!(ok, "a valid program must compile; stderr:\n{err}");
}
