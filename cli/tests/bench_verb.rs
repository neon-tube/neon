//! `neon bench` has to actually time `bench` blocks. A corpus test cannot see this —
//! the harness runs programs, and a bench binary's whole point is an entry that is not
//! `main` — so this drives the real binary and reads what it prints. Timing values are
//! machine noise; what is asserted is the protocol: a measurement line per bench, a
//! dead bench failing the verb, and `test` blocks staying invisible to it.

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

fn neon_bench(tag: &str, src: &str, extra: &[&str]) -> Run {
    let dir = std::env::temp_dir().join(format!("neon_bench_verb_{tag}"));
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join(format!("{tag}.neon"));
    std::fs::write(&file, src).expect("write source");

    let out = Command::new(neon())
        .env("NEON_SYSROOT", sysroot())
        .arg("bench")
        .arg(&file)
        .args(extra)
        .output()
        .expect("run neon bench");
    Run {
        ok: out.status.success(),
        out: String::from_utf8_lossy(&out.stdout).into_owned(),
        err: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

const FIXTURE: &str = "\
use std::list;

bench \"sums a range\" {
    assert(0..100 |> list::sum() == 4950);
}

bench \"dead on arrival\" {
    assert(1 == 2);
}

test \"not a bench\" {
    assert(true);
}
";

#[test]
fn benches_report_and_a_dead_one_fails_the_verb() {
    let run = neon_bench("mixed", FIXTURE, &[]);
    assert!(
        !run.ok,
        "a dead bench must fail the verb:\n{}{}",
        run.out, run.err
    );
    assert!(
        run.out.contains("bench sums a range ... ") && run.out.contains("/iter"),
        "a live bench reports a measurement:\n{}",
        run.out
    );
    assert!(
        run.out.contains("bench dead on arrival ... FAILED"),
        "{}",
        run.out
    );
    assert!(
        !run.out.contains("not a bench"),
        "test blocks are not benches:\n{}",
        run.out
    );
}

#[test]
fn filter_selects_and_the_verb_passes() {
    let run = neon_bench("filtered", FIXTURE, &["--filter", "range"]);
    assert!(
        run.ok,
        "the filtered run holds only the live bench:\n{}{}",
        run.out, run.err
    );
    assert!(run.out.contains("running 1 bench\n"), "{}", run.out);
    assert!(run.out.contains("/iter"), "{}", run.out);
}
