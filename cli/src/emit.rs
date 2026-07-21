//! The back half of every build verb: take a checked program, run the IR pipeline, emit C
//! next to the output, and hand it to the configured C compiler along with the runtime.

use crate::buildcfg::BuildConfig;
use crate::frontend::Checked;
use crate::sysroot::Sysroot;
use color_eyre::eyre::{bail, eyre, Result};
use neon_compiler::backend::c;
use neon_compiler::ir::{self, Stage};
use std::path::Path;
use std::process::Command;

/// Lower a checked program to an executable at `out`, writing a sibling `.c` file.
pub fn to_executable(checked: &Checked, out: &Path, cfg: &BuildConfig) -> Result<()> {
    let libs: Vec<(Vec<String>, &_)> =
        checked.libs.iter().map(|(p, m)| (p.clone(), m)).collect();
    let program = ir::compile(&checked.env, &checked.result, &checked.module, &libs, Stage::Final);
    link(&c::emit(&program), out, cfg)
}

/// Lower a checked program to a *test* executable: `test` blocks become functions and the
/// entry point dispatches to one of them, instead of `main` being the entry point. The
/// build configuration is the ordinary one, so a test runs against the same codegen a real
/// program gets.
pub fn to_test_executable(
    checked: &Checked,
    tests: &[neon_compiler::ir::lower::TestEntry],
    out: &Path,
    cfg: &BuildConfig,
) -> Result<()> {
    let libs: Vec<(Vec<String>, &_)> =
        checked.libs.iter().map(|(p, m)| (p.clone(), m)).collect();
    let program = ir::compile_tests(&checked.env, &checked.result, &checked.module, &libs);
    link(&c::emit_tests(&program, tests), out, cfg)
}

/// Write the emitted C beside `out` and hand it to the configured C compiler along with the
/// runtime archive.
fn link(c_source: &str, out: &Path, cfg: &BuildConfig) -> Result<()> {
    let c_file = out.with_extension("c");
    std::fs::write(&c_file, c_source).map_err(|e| eyre!("writing {}: {e}", c_file.display()))?;

    // The runtime is a prebuilt archive, not a pile of `.c` files: one variant per build
    // shape, built once by cmake (`runtime/CMakeLists.txt`). A build used to recompile
    // all eleven runtime translation units every time, and a shipped toolchain would have
    // had to ship the runtime's C source. `runtime_variant` decides which archive, and
    // refuses rather than substituting one — see its doc comment for why a sanitized
    // build may not link an uninstrumented runtime.
    let sysroot = Sysroot::find()?;
    let variant = cfg.runtime_variant()?;
    // The flavor matching the `cc` that links, so the archive's LTO bitcode is readable
    // and (for the sanitized variant) the sanitizer runtimes agree. A settled-for
    // fallback prints its warning below, same policy as sanitizer widening: allowed,
    // never silent.
    let archive = sysroot.runtime_lib(variant, cfg.cc_flavor()?)?;
    // Asking for a strict subset of the sanitized archive's sanitizers links the full set
    // instead — safe, but not something to do behind the user's back.
    if let Some(note) = cfg.sanitizer_widening_note(variant) {
        eprintln!("{note}");
    }
    if let Some(note) = &archive.note {
        eprintln!("{note}");
    }
    // The build passes `-flto` but the archive it links carries no LTO material for this
    // `cc` to inline through. Correct, just slow — and otherwise silent, which is exactly
    // how a no-LTO runtime once shipped on Apple Clang (which rejected the fat-objects
    // flag the archive was built with). A warning, never a failure: same policy as above.
    if cfg.uses_lto() && !crate::sysroot::archive_has_lto(&archive.path) {
        eprintln!(
            "warning: this build passes `-flto`, but the runtime archive at {} carries no \
             LTO bitcode, so the runtime's primitives (`neon_list_at`, retain/release, \
             element writes) stay un-inlinable in hot loops — measured ~2.5x on tight code. \
             Rebuild or reinstall the toolchain so its archive for `{}` carries LTO \
             material (a from-source `cargo build --release` on this machine does).",
            archive.path.display(),
            cfg.cc,
        );
    }
    let mut cmd = Command::new(&cfg.cc);
    cmd.args(cfg.cc_args(variant))
        .arg("-o")
        .arg(out)
        .arg(&c_file)
        // After the object that references it: a static archive only contributes the
        // members that resolve symbols already seen.
        .arg(&archive.path)
        .arg("-I")
        .arg(sysroot.include());
    let status = cmd
        .status()
        .map_err(|e| eyre!("could not run the C compiler `{}`: {e}", cfg.cc))?;
    if !status.success() {
        bail!("the C compiler failed on {}", c_file.display());
    }
    Ok(())
}
