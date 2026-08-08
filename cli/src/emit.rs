//! The back half of every build verb: take a checked program, run the IR pipeline, emit C
//! next to the output, and hand it to the configured C compiler along with the runtime.

use crate::buildcfg::BuildConfig;
use crate::frontend::Checked;
use crate::sysroot::Sysroot;
use color_eyre::eyre::{bail, eyre, Result};
use neon_compiler::backend::c;
use neon_compiler::ir::{self, Stage};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The name the linker will actually produce for a program the caller wants at `base`.
///
/// Everywhere but Windows that is `base` itself. On Windows a file without `.exe` is not
/// executable, and the C compiler appends the suffix whether or not `-o` asked for it — so
/// a caller that then tries to spawn the extensionless path is looking for a file nobody
/// wrote. Deciding it here, once, keeps the path the build verbs hand to `to_executable`
/// and the path they later run the same string.
///
/// An explicit `-o something.exe` is left alone rather than turned into `.exe.exe`; the
/// comparison is case-insensitive because Windows treats `.EXE` as the same suffix.
pub fn executable_path(base: PathBuf) -> PathBuf {
    let suffix = std::env::consts::EXE_SUFFIX;
    if suffix.is_empty() || base.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")) {
        return base;
    }
    let mut name = base.clone().into_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

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
    let flavor = cfg.cc_flavor()?;
    // An explicit `--runtime` wins over the sysroot's choice. The sysroot picks by `cc`
    // FLAVOUR for the host, which is right for a host build and has no answer at all for a
    // cross one: an archive built by `x86_64-w64-mingw32-gcc` is neither the gcc nor the
    // clang the sysroot knows about. Overriding is what lets a Windows binary be produced
    // (and run under Wine) from here, so a `@cfg(windows)` branch can be executed rather
    // than only type-checked.
    let archive = match &cfg.runtime {
        Some(path) => crate::sysroot::ResolvedRuntime { path: path.clone(), note: None },
        None => sysroot.runtime_lib(variant, flavor)?,
    };
    // Asking for a strict subset of the sanitized archive's sanitizers links the full set
    // instead — safe, but not something to do behind the user's back.
    if let Some(note) = cfg.sanitizer_widening_note(variant) {
        eprintln!("{note}");
    }
    if let Some(note) = &archive.note {
        eprintln!("{note}");
    }
    // The build passes `-flto` but the archive it links carries no LTO material *this*
    // family can inline through — LTO does not cross families, so a gcc archive's
    // `.gnu.lto_` is no use to a clang `cc`. Correct, just slow, and otherwise silent,
    // which is exactly how a no-LTO runtime once shipped on Apple Clang (which rejected
    // the fat-objects flag the archive was built with). A warning, never a failure: same
    // policy as above. Skipped when a flavor fallback already printed its note — that note
    // says the same thing about the same archive.
    if cfg.uses_lto() && archive.note.is_none() {
        if let Some(contents) = crate::sysroot::inspect_archive(&archive.path) {
            if !contents.lto_for(flavor) {
                eprintln!(
                    "warning: this build passes `-flto`, but the runtime archive at {} carries no \
                     LTO material `{}` can read, so the runtime's primitives (`neon_list_at`, \
                     retain/release, element writes) stay un-inlinable in hot loops — measured \
                     ~2.5x on tight code. Rebuild or reinstall the toolchain so its `{}` archive \
                     carries LTO material (a from-source `cargo build --release` on this machine \
                     does).",
                    archive.path.display(),
                    cfg.cc,
                    flavor.dir(),
                );
            }
        }
    }
    // `-l` libraries go AFTER the archive, for the same reason the archive goes after the
    // object: a traditional linker walks its inputs left to right and takes from a library
    // only what resolves a symbol it has already seen. `-lm` sat in the front-loaded flags,
    // so by the time `libneon_rt.a(math.c)` was pulled in and asked for `sqrt`, libm had
    // already been read and discarded. It went unnoticed for as long as nothing pulled
    // `math.c` out of the archive; a stdlib impl that calls `math::to_f64` is lowered into
    // every program, and every program stopped linking on a toolchain that does not quietly
    // add libm for you.
    //
    // The same applies to `--allocator`'s `-ljemalloc` and to any `-l` a caller puts in
    // `cflags`, so the split is by shape rather than by name.
    let (flags, libs) = split_link_libs(cfg.cc_args(variant));
    let mut cmd = Command::new(&cfg.cc);
    cmd.args(flags)
        .arg("-o")
        .arg(out)
        .arg(&c_file)
        // After the object that references it: a static archive only contributes the
        // members that resolve symbols already seen.
        .arg(&archive.path)
        .args(libs)
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

/// Split `cc` arguments into everything-else and the `-l` libraries, preserving order
/// within each group.
///
/// Both spellings move: `-lm` as one token, and `-l m` as the pair `normalize_cflags`
/// produces for a value-taking flag. A bare trailing `-l` with nothing after it is passed
/// through as a flag rather than swallowing the end of the list — `cc` will complain about
/// it, which is the right outcome and a better message than anything here would give.
fn split_link_libs(args: Vec<String>) -> (Vec<String>, Vec<String>) {
    let (mut flags, mut libs) = (Vec::new(), Vec::new());
    let mut it = args.into_iter().peekable();
    while let Some(a) = it.next() {
        if a == "-l" {
            match it.next() {
                Some(name) => {
                    libs.push(a);
                    libs.push(name);
                }
                None => flags.push(a),
            }
        } else if a.starts_with("-l") {
            libs.push(a);
        } else {
            flags.push(a);
        }
    }
    (flags, libs)
}

#[cfg(test)]
mod link_order_tests {
    use super::split_link_libs;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn libraries_move_and_the_rest_keeps_its_order() {
        let (flags, libs) = split_link_libs(v(&["-O2", "-lm", "-fsanitize=address", "-ljemalloc"]));
        assert_eq!(flags, v(&["-O2", "-fsanitize=address"]));
        assert_eq!(libs, v(&["-lm", "-ljemalloc"]));
    }

    #[test]
    fn the_separated_spelling_moves_as_a_pair() {
        let (flags, libs) = split_link_libs(v(&["-l", "m", "-O2"]));
        assert_eq!(flags, v(&["-O2"]));
        assert_eq!(libs, v(&["-l", "m"]));
    }

    #[test]
    fn a_dangling_l_is_left_for_cc_to_reject() {
        let (flags, libs) = split_link_libs(v(&["-O2", "-l"]));
        assert_eq!(flags, v(&["-O2", "-l"]));
        assert!(libs.is_empty());
    }

    /// `-L` is a search PATH, not a library: it must stay in front, where it can inform the
    /// lookup of the `-l`s that now follow the archive.
    #[test]
    fn a_search_path_is_not_a_library() {
        let (flags, libs) = split_link_libs(v(&["-L/opt/lib", "-lm"]));
        assert_eq!(flags, v(&["-L/opt/lib"]));
        assert_eq!(libs, v(&["-lm"]));
    }
}
