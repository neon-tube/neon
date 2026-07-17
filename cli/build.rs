//! Stages the sysroot next to the executable, in the layout an install uses:
//!
//!     dev                          installed
//!     target/<profile>/neon-cli    prefix/bin/neon
//!     target/<profile>/include/    prefix/include/
//!     target/<profile>/lib/        prefix/lib/
//!     target/<profile>/stdlib/     prefix/stdlib/
//!
//! so `Sysroot::find` resolves both the same way.

use std::path::{Path, PathBuf};

fn main() {
    let rt_root = PathBuf::from(
        std::env::var("DEP_NEON_RT_ROOT")
            .expect("neon-runtime's build script must publish cargo:root"),
    );
    let rt_include = PathBuf::from(
        std::env::var("DEP_NEON_RT_INCLUDE")
            .expect("neon-runtime's build script must publish cargo:include"),
    );

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let stdlib_src = manifest.join("../stdlib");
    let sysroot = target_dir();

    stage_tree(&rt_include, &sysroot.join("include"));
    stage_file(&rt_root.join("lib/libneon_rt.a"), &sysroot.join("lib/libneon_rt.a"));
    if stdlib_src.is_dir() {
        stage_tree(&stdlib_src, &sysroot.join("stdlib"));
    }

    println!("cargo:rerun-if-changed={}", stdlib_src.display());
    println!("cargo:rerun-if-env-changed=DEP_NEON_RT_ROOT");
}

/// `target/<profile>`. Cargo exposes no variable for it; OUT_DIR is
/// `target/<profile>/build/<pkg>-<hash>/out`, hence three levels up.
fn target_dir() -> PathBuf {
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let dir = out
        .ancestors()
        .nth(3)
        .expect("OUT_DIR should be target/<profile>/build/<pkg>-<hash>/out")
        .to_path_buf();
    assert!(
        dir.join("build").is_dir(),
        "derived target dir {} does not look like target/<profile>",
        dir.display()
    );
    dir
}

fn stage_file(from: &Path, to: &Path) {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    std::fs::copy(from, to)
        .unwrap_or_else(|e| panic!("stage {} -> {}: {e}", from.display(), to.display()));
}

/// Clears `to` first, so a deleted source file cannot linger and keep working.
fn stage_tree(from: &Path, to: &Path) {
    let _ = std::fs::remove_dir_all(to);
    copy_tree(from, to);
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create dir");
    for entry in std::fs::read_dir(from).unwrap_or_else(|e| panic!("read {}: {e}", from.display())) {
        let entry = entry.expect("dir entry");
        let dest = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &dest);
        } else {
            stage_file(&entry.path(), &dest);
        }
    }
}
