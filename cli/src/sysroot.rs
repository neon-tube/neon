use crate::buildcfg::{CcFlavor, RuntimeVariant};
use color_eyre::eyre::{bail, eyre, Result};
use std::path::PathBuf;

/// Locates `include/`, the `lib/<flavor>/libneon_rt*.a` archives and `stdlib/`.
///
/// Resolved at runtime, never baked in: a compile-time path describes the
/// machine that built the compiler, not the one running it.
pub struct Sysroot(PathBuf);

/// The archive flavors a sysroot may carry, in the order they are probed and reported.
const FLAVOR_DIRS: &[&str] = &["gcc", "clang"];

/// A runtime archive picked for a link, and the warning to show when the pick settled
/// for a fallback flavor (see `Sysroot::runtime_lib`).
pub struct ResolvedRuntime {
    pub path: PathBuf,
    pub note: Option<String>,
}

impl Sysroot {
    fn probe(dir: PathBuf) -> Option<Self> {
        // Any one flavor's release archive marks a sysroot: which flavors exist depends
        // on the compilers present when the toolchain was built, and needing a specific
        // one is a *link-time* question (`runtime_lib`), not an existence question.
        FLAVOR_DIRS
            .iter()
            .any(|f| dir.join("lib").join(f).join("libneon_rt.a").is_file())
            .then_some(Sysroot(dir))
    }

    pub fn find() -> Result<Self> {
        if let Some(dir) = std::env::var_os("NEON_SYSROOT") {
            let dir = PathBuf::from(dir);
            return Self::probe(dir.clone()).ok_or_else(|| {
                eyre!(
                    "NEON_SYSROOT is set to '{}' but there is no lib/<flavor>/libneon_rt.a there \
                     (flavors: gcc, clang)",
                    dir.display()
                )
            });
        }

        let exe = std::env::current_exe().map_err(|e| eyre!("cannot locate the neon binary: {e}"))?;
        let exe_dir = exe
            .parent()
            .ok_or_else(|| eyre!("the neon binary has no parent directory"))?;

        // exe_dir: dev (target/<profile>). exe_dir/..: installed (prefix/bin).
        let candidates = [exe_dir.to_path_buf(), exe_dir.join("..")];
        for dir in &candidates {
            if let Some(found) = Self::probe(dir.clone()) {
                return Ok(found);
            }
        }

        bail!(
            "cannot find the Neon sysroot: no lib/<flavor>/libneon_rt.a under {}.\n\
             Set NEON_SYSROOT to override.",
            candidates
                .iter()
                .map(|p| format!("'{}'", p.display()))
                .collect::<Vec<_>>()
                .join(" or ")
        )
    }

    /// The stdlib directory alone, for front-end runs that need no runtime.
    ///
    /// Probed independently of the runtime archives: type-checking needs only the
    /// stdlib source, and the runtime archive does not exist until the backend does,
    /// so requiring it here would make `neon check` unusable before codegen lands.
    ///
    /// Must accept the same two layouts as `find`: beside the binary
    /// (`exe_dir/stdlib`, e.g. `target/release`) and installed (`exe_dir/../stdlib`,
    /// `prefix/bin/neon` → `prefix/stdlib`). Probing rather than assuming one — the old
    /// code hard-coded the installed layout, so a beside-the-binary install that `find`
    /// happily located still failed every compile at `stdlib_dir`.
    pub fn stdlib_dir() -> Result<PathBuf> {
        if let Some(dir) = std::env::var_os("NEON_SYSROOT") {
            return Ok(PathBuf::from(dir).join("stdlib"));
        }
        let exe = std::env::current_exe().map_err(|e| eyre!("cannot locate the neon binary: {e}"))?;
        let exe_dir = exe.parent().ok_or_else(|| eyre!("the neon binary has no parent directory"))?;
        let candidates = [exe_dir.join("stdlib"), exe_dir.join("../stdlib")];
        if let Some(found) = candidates.iter().find(|d| d.is_dir()) {
            return Ok(found.clone());
        }
        bail!(
            "cannot find the Neon stdlib: no stdlib/ under {}.\n\
             Set NEON_SYSROOT to override.",
            candidates
                .iter()
                .map(|p| format!("'{}'", p.display()))
                .collect::<Vec<_>>()
                .join(" or ")
        )
    }

    pub fn root(&self) -> &PathBuf {
        &self.0
    }

    pub fn include(&self) -> PathBuf {
        self.0.join("include")
    }

    /// Where the prebuilt runtime archives live: one subdirectory per compiler flavor,
    /// each holding the three variants. `lib/<flavor>/libneon_rt.a` doubles as the
    /// marker `probe` looks for: it is the variant that always exists.
    pub fn lib_dir(&self) -> PathBuf {
        self.0.join("lib")
    }

    /// The flavor subdirectories actually present, for diagnostics.
    pub fn flavors_present(&self) -> Vec<&'static str> {
        FLAVOR_DIRS
            .iter()
            .copied()
            .filter(|f| self.lib_dir().join(f).join("libneon_rt.a").is_file())
            .collect()
    }

    /// The prebuilt archive for `variant`, preferring `flavor`'s compiler family, plus a
    /// warning when the build had to settle.
    ///
    /// A missing *variant* is never swapped for another variant — a sanitizer reports
    /// nothing about code compiled without it, so a silently downgraded runtime is a lie
    /// about what was built. A missing *flavor* splits by what the substitution would
    /// actually do:
    ///
    ///   - `Release`/`Debug`: the other flavor's archive links correctly — fat objects
    ///     carry real machine code — but the cross-family link cannot read the LTO
    ///     bitcode, so every runtime call stays un-inlinable (measured at 4× on a hot
    ///     loop). Allowed, with a warning that says exactly that, because a slower
    ///     correct build beats a refusal when the right compiler simply was not there
    ///     when the toolchain was built.
    ///   - `Sanitized`: refused. gcc's libasan and clang's compiler-rt are different
    ///     runtimes; one family's instrumented archive does not link under the other's
    ///     driver, so the fallback would not be a slower build — it would be a broken
    ///     link or worse.
    pub fn runtime_lib(&self, variant: RuntimeVariant, flavor: CcFlavor) -> Result<ResolvedRuntime> {
        let dir = self.lib_dir().join(flavor.dir());
        let path = dir.join(variant.archive());
        if path.is_file() {
            return Ok(ResolvedRuntime { path, note: None });
        }

        // The flavor was staged but this variant is missing: an incomplete toolchain,
        // and no substitution of any kind.
        if dir.join("libneon_rt.a").is_file() {
            let present: Vec<String> = [
                RuntimeVariant::Release,
                RuntimeVariant::Debug,
                RuntimeVariant::Sanitized,
            ]
            .iter()
            .map(|v| v.archive())
            .filter(|a| dir.join(a).is_file())
            .map(str::to_string)
            .collect();
            bail!(
                "this build needs the runtime archive `{}/{}`, which is not in the sysroot at {}.\n\
                 Present there: {}.\n\
                 The toolchain's runtime is incomplete; rebuild or reinstall it. Another \
                 variant will not be substituted — it would change what the build actually \
                 links without saying so.",
                flavor.dir(),
                variant.archive(),
                self.0.display(),
                if present.is_empty() { "nothing".into() } else { present.join(", ") },
            )
        }

        // The whole flavor is missing: the toolchain was built on a machine without that
        // compiler family.
        let fallback = self
            .flavors_present()
            .into_iter()
            .find(|f| self.lib_dir().join(f).join(variant.archive()).is_file());
        let Some(other) = fallback else {
            bail!(
                "the sysroot at {} has no `{}` runtime archives and no other flavor \
                 carrying `{}` to fall back on. Rebuild or reinstall the toolchain.",
                self.0.display(),
                flavor.dir(),
                variant.archive(),
            )
        };

        if variant == RuntimeVariant::Sanitized {
            bail!(
                "this build's C compiler is {} but the sysroot at {} carries no {} runtime \
                 archives (present: {other}).\n\
                 The {other} sanitized archive cannot stand in: gcc's libasan and clang's \
                 compiler-rt are different sanitizer runtimes, and one family's \
                 instrumented archive does not link under the other's driver. Use a {} \
                 compiler via `--cc`/`$CC`, or rebuild the toolchain with {} installed.",
                flavor.dir(),
                self.0.display(),
                flavor.dir(),
                other,
                flavor.dir(),
            )
        }

        Ok(ResolvedRuntime {
            path: self.lib_dir().join(other).join(variant.archive()),
            note: Some(format!(
                "warning: this build's C compiler is {} but the toolchain carries no {} \
                 runtime archives; linking the {other} one instead. It links correctly, \
                 but a cross-family link cannot read the archive's LTO bitcode, so \
                 runtime primitives stay un-inlinable in hot code. For full speed, \
                 rebuild the toolchain with {} installed or switch `--cc`/`$CC` to {other}.",
                flavor.dir(),
                flavor.dir(),
                flavor.dir(),
            )),
        })
    }

    pub fn stdlib(&self) -> PathBuf {
        self.0.join("stdlib")
    }
}

/// Whether a runtime archive carries LTO material a matching `cc` can inline through. A
/// build that passes `-flto` against an archive that does not is not an error — it links
/// and runs correctly — but the runtime's primitives stay un-inlinable in hot loops, which
/// on tight code is a ~2.5x regression with no other symptom. `emit::link` warns on it.
///
/// A raw byte scan rather than shelling out to `ar`/`nm`/`llvm-bcanalyzer`: it needs no
/// tools present on the user's machine, and the two markers are unambiguous. Both ship
/// shapes are covered — clang emits LLVM bitcode (checked against gcc-15 and Apple Clang
/// archives here), gcc emits fat objects whose ELF section names carry `.gnu.lto_`:
///   * LLVM bitcode: the wrapper magic `0x0B17C0DE` (little-endian `DE C0 17 0B`) or the
///     raw bitcode magic `BC\xC0\xDE`.
///   * GCC fat LTO: the literal section-name substring `.gnu.lto_`.
/// A read error returns `true`: the warning must never itself break a build, and assuming
/// an archive is fine is the safe direction for a diagnostic.
pub fn archive_has_lto(path: &std::path::Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return true;
    };
    const LLVM_WRAPPER: &[u8] = &[0xDE, 0xC0, 0x17, 0x0B];
    const LLVM_RAW: &[u8] = &[0x42, 0x43, 0xC0, 0xDE];
    const GCC_FAT: &[u8] = b".gnu.lto_";
    bytes.windows(LLVM_WRAPPER.len()).any(|w| w == LLVM_WRAPPER)
        || bytes.windows(LLVM_RAW.len()).any(|w| w == LLVM_RAW)
        || bytes.windows(GCC_FAT.len()).any(|w| w == GCC_FAT)
}
