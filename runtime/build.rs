//! LOCATOR, not builder. The archives are produced by `cargo make rt` (tools/rt.sh, a
//! plain cmake invocation per compiler family) into `target/neon-rt/<flavor>/`; this
//! script only verifies they are present and FRESH, and publishes their location. It
//! never invokes anything — which is what makes `cargo check` and rust-analyzer instant,
//! where the previous version configured and built two cmake trees on every rerun.
//!
//! Freshness is a refusal, not a rebuild: an archive older than the C sources it was
//! built from does not get silently used (a runtime change measured as having no effect,
//! because it was never linked, once cost an afternoon) — the build stops and names the
//! command. The one thing this script must therefore never do is guess.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

fn main() {
    // Rerun when the C changes (to re-check freshness), when a stamp changes (a new `rt`
    // run), or when the override moves.
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=include");
    println!("cargo:rerun-if-changed=CMakeLists.txt");
    println!("cargo:rerun-if-changed=flags");
    println!("cargo:rerun-if-env-changed=NEON_RT_DIST");

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let dist = std::env::var("NEON_RT_DIST")
        .map(PathBuf::from)
        .unwrap_or_else(|_| manifest.join("../target/neon-rt"));

    let newest_source = newest_mtime(&manifest.join("src"))
        .max(newest_mtime(&manifest.join("include")))
        .max(mtime(&manifest.join("CMakeLists.txt")))
        .max(newest_mtime(&manifest.join("flags")));

    let mut found: Vec<String> = Vec::new();
    for flavor in ["gcc", "clang"] {
        let dir = dist.join(flavor);
        let stamp = dir.join(".stamp");
        if !dir.join("lib/libneon_rt.a").is_file() {
            continue;
        }
        println!("cargo:rerun-if-changed={}", stamp.display());
        if mtime(&stamp) < newest_source {
            panic!(
                "the {flavor} runtime archives under {} are STALE (the C sources are \
                 newer). Run `cargo make rt` — this build will not link an archive that \
                 no longer matches the source.",
                dir.display()
            );
        }
        found.push(flavor.to_string());
    }
    assert!(
        !found.is_empty(),
        "no runtime archives under {} — run `cargo make rt` first (the archives are \
         built by the top-level cargo-make, not by cargo)",
        dist.display()
    );

    // `links = "neon_rt"` turns these into DEP_NEON_RT_{ROOT,INCLUDE} for dependents'
    // build scripts. ROOT holds one subdirectory per flavor staged; the headers are
    // compiler-independent, so INCLUDE points into whichever flavor exists.
    println!("cargo:root={}", dist.display());
    println!("cargo:include={}/{}/include", dist.display(), found[0]);
}

fn mtime(p: &Path) -> SystemTime {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn newest_mtime(dir: &Path) -> SystemTime {
    let mut newest = SystemTime::UNIX_EPOCH;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return newest;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let t = if path.is_dir() {
            newest_mtime(&path)
        } else {
            mtime(&path)
        };
        newest = newest.max(t);
    }
    newest
}
