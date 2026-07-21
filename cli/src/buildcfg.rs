//! How a program is compiled: the C compiler, optimisation, sanitizers, the allocator,
//! and any extra flags. Layered lowest-to-highest: built-in defaults, then a
//! `[build]` table in `neon.toml`, then command-line flags. So a project pins its
//! defaults and a single build overrides them.

use color_eyre::eyre::{eyre, Result};
use serde::Deserialize;
use std::path::Path;

/// A build preset: the one knob that sets optimisation, debug info, and the trimmings.
/// `opt` and `debug_symbols` refine it.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
    /// `-O0`, debug symbols, and the runtime's debug assertions (`NEON_DEBUG`).
    Debug,
    /// `-O3`.
    Release,
    /// `-O3` plus every go-fast switch: LTO, `-march=native`, no frame pointer, `NDEBUG`.
    OptRelease,
}

impl Mode {
    /// The `-O` level this mode implies (an explicit `opt` overrides it).
    fn opt_level(self) -> &'static str {
        match self {
            Mode::Debug => "0",
            Mode::Release | Mode::OptRelease => "3",
        }
    }
    /// A debug build turns on the runtime's assertions and symbols.
    fn is_debug(self) -> bool {
        matches!(self, Mode::Debug)
    }
    fn parse(name: &str) -> Result<Mode> {
        match name {
            "debug" => Ok(Mode::Debug),
            "release" => Ok(Mode::Release),
            "opt_release" | "opt-release" => Ok(Mode::OptRelease),
            other => Err(eyre!("unknown mode `{other}` (debug, release, opt_release)")),
        }
    }
}

/// Which prebuilt runtime archive a build links. The runtime is compiled once, by cmake,
/// into one archive per build shape (see `runtime/CMakeLists.txt`); a Neon build picks one
/// rather than recompiling the runtime's C sources.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RuntimeVariant {
    /// `libneon_rt.a` — `-O3`.
    Release,
    /// `libneon_rt_debug.a` — `-O0 -g -DNEON_DEBUG`.
    Debug,
    /// `libneon_rt_san.a` — `-fsanitize=address,undefined -O1 -g -fno-omit-frame-pointer`.
    Sanitized,
}

/// The sanitizers the prebuilt sanitized archive is instrumented for. Asking for anything
/// outside this set is an error, never a fallback — see `runtime_variant`.
pub const SANITIZED_VARIANT_COVERS: &[&str] = &["address", "undefined"];

/// Which compiler *family* the resolved `cc` belongs to, which decides the runtime
/// archive flavor under `lib/<flavor>/`. Family matters twice: LTO bitcode does not
/// cross families (a clang link against gcc fat objects silently drops to the machine
/// code and loses every cross-archive inline — measured at 4× on the n-body benchmark),
/// and the two sanitizer runtimes do not link against each other's instrumentation at
/// all. So the archive must be built by the same family that links the program.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CcFlavor {
    Gcc,
    Clang,
}

impl CcFlavor {
    /// The subdirectory of the sysroot's `lib/` holding this flavor's archives.
    pub fn dir(self) -> &'static str {
        match self {
            CcFlavor::Gcc => "gcc",
            CcFlavor::Clang => "clang",
        }
    }
}

impl RuntimeVariant {
    /// The archive's file name under the sysroot's `lib/`.
    pub fn archive(self) -> &'static str {
        match self {
            RuntimeVariant::Release => "libneon_rt.a",
            RuntimeVariant::Debug => "libneon_rt_debug.a",
            RuntimeVariant::Sanitized => "libneon_rt_san.a",
        }
    }

    /// The sanitizers this archive's objects were **compiled** with, which is also exactly
    /// the set the final link must enable.
    ///
    /// Not "the set the user asked for": a sanitizer's instrumentation is call sites into
    /// its runtime library, so an archive built `-fsanitize=address,undefined` contains
    /// unresolved references to *both* runtimes. Linking it while passing only a subset
    /// leaves the other's symbols undefined and the link fails outright —
    /// `undefined reference to __ubsan_handle_type_mismatch_v1` for `address` alone, and
    /// symmetrically `__asan_report_load4` for `undefined` alone. A proper subset of an
    /// archive's instrumentation is not linkable against that archive.
    ///
    /// So the link takes its `-fsanitize` from here rather than from `BuildConfig::
    /// sanitize`, which makes the mismatch unrepresentable: the archive and the flags that
    /// link it are now derived from one value instead of two that could drift apart.
    pub fn link_sanitizers(self) -> &'static [&'static str] {
        match self {
            RuntimeVariant::Release | RuntimeVariant::Debug => &[],
            RuntimeVariant::Sanitized => SANITIZED_VARIANT_COVERS,
        }
    }
}

/// The malloc implementation to link. Swapping it is a link-time interposition.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Allocator {
    System,
    Jemalloc,
    Mimalloc,
    Tcmalloc,
}

impl Allocator {
    /// The linker flags that interpose this allocator over `malloc`.
    fn link_flags(self) -> Vec<String> {
        match self {
            Allocator::System => vec![],
            Allocator::Jemalloc => vec!["-ljemalloc".into()],
            Allocator::Mimalloc => vec!["-lmimalloc".into()],
            Allocator::Tcmalloc => vec!["-ltcmalloc".into()],
        }
    }
}

/// Overrides a build takes from the command line. `None` means "leave the layer below".
#[derive(Default)]
pub struct BuildFlags {
    pub cc: Option<String>,
    pub mode: Option<Mode>,
    pub opt: Option<String>,
    pub debug_symbols: Option<bool>,
    pub sanitize: Vec<String>,
    pub allocator: Option<Allocator>,
    pub stacktrace: Option<bool>,
    /// Arbitrary flags passed straight through to the C compiler (`-C`).
    pub cflags: Vec<String>,
}

/// The `[build]` table in `neon.toml`.
#[derive(Default, Deserialize)]
struct TomlBuild {
    cc: Option<String>,
    mode: Option<String>,
    opt: Option<String>,
    debug_symbols: Option<bool>,
    #[serde(default)]
    sanitize: Vec<String>,
    allocator: Option<String>,
    stacktrace: Option<bool>,
    #[serde(default)]
    cflags: Vec<String>,
}

#[derive(Default, Deserialize)]
struct TomlFile {
    #[serde(default)]
    build: TomlBuild,
}

/// The resolved build configuration.
pub struct BuildConfig {
    pub cc: String,
    pub mode: Mode,
    /// An explicit `-O` level overriding the mode's default, if any.
    pub opt: Option<String>,
    /// Emit `-g`. Always on in debug mode.
    pub debug_symbols: bool,
    pub sanitize: Vec<String>,
    pub allocator: Allocator,
    /// Keep frames walkable so a `throw` can capture one. Mutually exclusive with
    /// `opt-release`'s `-fomit-frame-pointer`: this wins where they meet.
    pub stacktrace: bool,
    pub cflags: Vec<String>,
}

impl BuildConfig {
    /// Resolve defaults, then `neon.toml` (searched upward from `near`), then flags.
    pub fn resolve(near: &Path, flags: BuildFlags) -> Result<Self> {
        let mut cfg = BuildConfig {
            cc: std::env::var("CC").unwrap_or_else(|_| "cc".into()),
            mode: Mode::Release,
            opt: None,
            debug_symbols: false,
            sanitize: vec![],
            allocator: Allocator::System,
            stacktrace: false,
            cflags: vec![],
        };

        if let Some(toml) = find_manifest(near)? {
            let b = toml.build;
            if let Some(cc) = b.cc {
                cfg.cc = cc;
            }
            if let Some(m) = b.mode {
                cfg.mode = Mode::parse(&m)?;
            }
            if b.opt.is_some() {
                cfg.opt = b.opt;
            }
            if let Some(d) = b.debug_symbols {
                cfg.debug_symbols = d;
            }
            cfg.sanitize.extend(b.sanitize);
            if let Some(a) = b.allocator {
                cfg.allocator = parse_allocator(&a)?;
            }
            if let Some(t) = b.stacktrace {
                cfg.stacktrace = t;
            }
            cfg.cflags.extend(b.cflags);
        }

        // Command-line flags win.
        if let Some(cc) = flags.cc {
            cfg.cc = cc;
        }
        if let Some(m) = flags.mode {
            cfg.mode = m;
        }
        if flags.opt.is_some() {
            cfg.opt = flags.opt;
        }
        if let Some(d) = flags.debug_symbols {
            cfg.debug_symbols = d;
        }
        cfg.sanitize.extend(flags.sanitize);
        if let Some(t) = flags.stacktrace {
            cfg.stacktrace = t;
        }
        if let Some(a) = flags.allocator {
            cfg.allocator = a;
        }
        cfg.cflags.extend(flags.cflags);
        Ok(cfg)
    }

    /// Which prebuilt runtime archive this build must link.
    ///
    /// Sanitizers decide first, and they decide strictly. Measured: ASan does **not**
    /// report a heap-buffer-overflow that happens inside code compiled without
    /// `-fsanitize` — an uninstrumented archive linked into a sanitized program reports
    /// nothing and exits 0. So a request for sanitizers the prebuilt archive does not
    /// carry is an error. There is no fallback to an uninstrumented runtime, silent or
    /// otherwise: a sanitized build that quietly covers only half the program is worse
    /// than a build that refuses.
    ///
    /// `OptRelease` links the plain release archive. It cannot have its own variant: the
    /// mode's distinguishing flags are `-flto` and `-march=native`, and neither survives
    /// prebuilding — an archive is compiled once, on the toolchain's build machine, for a
    /// target it does not know, by a compiler that is not the `cc` doing the final link.
    /// So under `opt-release` the *program* is still `-O3 -flto -march=native`, and the
    /// runtime is plain `-O3`. This is a real, deliberate regression from compiling the
    /// runtime per build; it is written down here rather than left to be discovered.
    pub fn runtime_variant(&self) -> Result<RuntimeVariant> {
        // `--sanitize address,undefined` and `--sanitize address --sanitize undefined`
        // are the same request.
        let requested = self.requested_sanitizers();

        if requested.is_empty() {
            return Ok(match self.mode {
                Mode::Debug => RuntimeVariant::Debug,
                Mode::Release | Mode::OptRelease => RuntimeVariant::Release,
            });
        }

        let unsupported: Vec<&str> = requested
            .iter()
            .copied()
            .filter(|s| !SANITIZED_VARIANT_COVERS.contains(s))
            .collect();
        if unsupported.is_empty() {
            return Ok(RuntimeVariant::Sanitized);
        }

        Err(eyre!(
            "no prebuilt runtime is instrumented for the sanitizer{} {}.\n\
             The runtime ships three prebuilt variants:\n  \
               libneon_rt.a        (release, -O3)\n  \
               libneon_rt_debug.a  (debug, -O0 -g -DNEON_DEBUG)\n  \
               libneon_rt_san.a    (sanitized, -fsanitize={})\n\
             Linking an uninstrumented runtime into a sanitized program is not a \
             fallback: a sanitizer sees nothing that happens inside uninstrumented code, \
             so the build would silently cover only your program and not the runtime it \
             calls. Drop the unsupported sanitizer{}, or rebuild the runtime with it.",
            if unsupported.len() == 1 { "" } else { "s" },
            unsupported.iter().map(|s| format!("`{s}`")).collect::<Vec<_>>().join(", "),
            SANITIZED_VARIANT_COVERS.join(","),
            if unsupported.len() == 1 { "" } else { "s" },
        ))
    }

    /// Whether this build passes `-flto`, and so expects the runtime archive to carry LTO
    /// bitcode it can inline through. The single source of truth for that condition —
    /// `cc_args` emits `-flto` exactly when this is true, and the link path checks the
    /// archive can honor it against the same predicate (see `emit::link`).
    pub fn uses_lto(&self) -> bool {
        !self.mode.is_debug()
    }

    /// Which archive flavor this build's `cc` needs, probed from `cc --version` rather
    /// than the name — `cc` is usually a symlink, and on macOS `gcc` *is* clang. Not
    /// cached: it runs once per build, at link time, and a probe that cannot run at all
    /// is the right moment to say the compiler is missing.
    pub fn cc_flavor(&self) -> Result<CcFlavor> {
        let output = std::process::Command::new(&self.cc)
            .arg("--version")
            .output()
            .map_err(|e| eyre!("cannot run `{} --version` to identify the compiler: {e}", self.cc))?;
        let text = String::from_utf8_lossy(&output.stdout).to_lowercase();
        Ok(if text.contains("clang") { CcFlavor::Clang } else { CcFlavor::Gcc })
    }

    /// The sanitizers requested, normalised: split on commas, trimmed, deduplicated.
    fn requested_sanitizers(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for s in self.sanitize.iter().flat_map(|s| s.split(',')).map(str::trim) {
            if !s.is_empty() && !out.contains(&s) {
                out.push(s);
            }
        }
        out
    }

    /// A message when the build enables more sanitizers than were asked for, or `None`
    /// when it enables exactly what was asked.
    ///
    /// This widening is deliberate but it must not be invisible. Asking for `address`
    /// alone gets UBSan as well, because the only sanitized runtime archive carries both
    /// and a subset will not link against it (see `RuntimeVariant::link_sanitizers`).
    /// Widening is the safe direction — it can only ever add checking, never remove it,
    /// so a build is never quietly less checked than requested — but it does mean the
    /// build does something other than what was typed, and a UBSan report on the user's
    /// own code would otherwise arrive with no explanation of where UBSan came from.
    pub fn sanitizer_widening_note(&self, variant: RuntimeVariant) -> Option<String> {
        let requested = self.requested_sanitizers();
        let effective = variant.link_sanitizers();
        let added: Vec<&str> = effective
            .iter()
            .copied()
            .filter(|s| !requested.contains(s))
            .collect();
        if requested.is_empty() || added.is_empty() {
            return None;
        }
        Some(format!(
            "note: also enabling `{}`: the sanitized runtime ({}) is built \
             `-fsanitize={}`, and linking it with only `{}` would leave the other \
             sanitizer's runtime symbols undefined. This adds checking, never removes it.",
            added.join(","),
            variant.archive(),
            effective.join(","),
            requested.join(","),
        ))
    }

    /// The full argument list for the C compiler (excluding sources and `-o`, which the
    /// caller supplies).
    ///
    /// Takes the runtime variant because the two must agree: the `-fsanitize` flags here
    /// are the ones `variant`'s archive was compiled with, not the ones the user typed.
    /// See `RuntimeVariant::link_sanitizers`.
    ///
    /// Note that these flags otherwise describe the *program* only. The runtime is a
    /// prebuilt archive, so `cflags` and `-D`s passed here no longer reach the runtime's
    /// translation units.
    pub fn cc_args(&self, variant: RuntimeVariant) -> Vec<String> {
        let mut args = vec!["-std=c11".to_string()];

        // Optimisation: an explicit `opt` wins, otherwise the mode's level.
        let opt = self.opt.clone().unwrap_or_else(|| self.mode.opt_level().to_string());
        args.push(format!("-O{opt}"));

        // Debug info when asked, or always in debug mode; debug mode also turns on the
        // runtime's assertions and makes a trap `abort()` for a debugger.
        if self.debug_symbols || self.mode.is_debug() {
            args.push("-g".into());
        }
        if self.mode.is_debug() {
            args.push("-DNEON_DEBUG".into());
        }

        // Link-time optimisation from `release` up. The runtime is a separate translation
        // unit, so without it every retain, release and element access stays an
        // un-inlinable call — and measured on this workload LTO is both *faster to build*
        // and better optimised than not having it. `debug` skips it: at -O0 there is no
        // inlining to gain, and LTO degrades the debug info that mode exists for.
        if self.uses_lto() {
            args.push("-flto".into());
        }
        // `opt-release` trims the frame pointer, which is exactly what a stacktrace needs
        // to walk. The two are mutually exclusive and the trace wins: an explicit
        // `-fno-omit-frame-pointer` rather than merely dropping the flag, because `-O3`
        // omits frame pointers on most targets on its own.
        if matches!(self.mode, Mode::OptRelease) {
            args.extend(["-march=native", "-DNDEBUG"].map(String::from));
            if !self.stacktrace {
                args.push("-fomit-frame-pointer".into());
            }
        }
        if self.stacktrace {
            args.push("-fno-omit-frame-pointer".into());
        }

        // From the archive, not from `self.sanitize`: a subset does not link (see
        // `RuntimeVariant::link_sanitizers`). One `-fsanitize=a,b` rather than one flag
        // each, so the link matches how the archive was compiled exactly.
        if !variant.link_sanitizers().is_empty() {
            args.push(format!("-fsanitize={}", variant.link_sanitizers().join(",")));
        }
        args.extend(self.allocator.link_flags());
        // `std::math` bottoms out in libm, which is a separate library on Linux (it is
        // folded into libc on macOS, where this is a harmless no-op).
        args.push("-lm".into());
        args.extend(normalize_cflags(&self.cflags));
        args
    }
}

/// Turn raw cflag entries into the argument list `cc` expects: split space-separated
/// entries into individual tokens (`-C "-flto -march=native"` → two args), trim, drop
/// empties, and drop duplicates. A value-taking flag and its argument (`-I inc`) dedupe
/// and move as a pair, so distinct `-I a -I b` both survive while `-flto -flto` collapses.
fn normalize_cflags(cflags: &[String]) -> Vec<String> {
    const VALUE_FLAGS: &[&str] = &[
        "-I", "-D", "-U", "-L", "-l", "-include", "-isystem", "-x", "-Xlinker",
        "-Xpreprocessor", "-framework",
    ];
    let tokens: Vec<&str> = cflags.iter().flat_map(|c| c.split_whitespace()).collect();
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        if VALUE_FLAGS.contains(&t) && i + 1 < tokens.len() {
            let pair = format!("{t} {}", tokens[i + 1]);
            if seen.insert(pair) {
                out.push(t.to_string());
                out.push(tokens[i + 1].to_string());
            }
            i += 2;
        } else {
            if seen.insert(t.to_string()) {
                out.push(t.to_string());
            }
            i += 1;
        }
    }
    out
}

fn parse_allocator(name: &str) -> Result<Allocator> {
    match name {
        "system" => Ok(Allocator::System),
        "jemalloc" => Ok(Allocator::Jemalloc),
        "mimalloc" => Ok(Allocator::Mimalloc),
        "tcmalloc" => Ok(Allocator::Tcmalloc),
        other => Err(eyre!("unknown allocator `{other}` (system, jemalloc, mimalloc, tcmalloc)")),
    }
}

/// Find and parse `neon.toml`, searching from `near`'s directory upward.
fn find_manifest(near: &Path) -> Result<Option<TomlFile>> {
    let mut dir = if near.is_dir() { Some(near) } else { near.parent() };
    while let Some(d) = dir {
        let candidate = d.join("neon.toml");
        if candidate.is_file() {
            let src = std::fs::read_to_string(&candidate)?;
            return Ok(Some(toml::from_str(&src).map_err(|e| eyre!("{}: {e}", candidate.display()))?));
        }
        dir = d.parent();
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(mode: Mode, stacktrace: bool) -> BuildConfig {
        BuildConfig {
            cc: "cc".into(),
            mode,
            opt: None,
            debug_symbols: false,
            sanitize: vec![],
            allocator: Allocator::System,
            stacktrace,
            cflags: vec![],
        }
    }

    fn with_sanitize(mode: Mode, sanitize: &[&str]) -> BuildConfig {
        let mut c = cfg(mode, false);
        c.sanitize = sanitize.iter().map(|s| s.to_string()).collect();
        c
    }

    #[test]
    fn runtime_variant_follows_mode_when_unsanitized() {
        assert_eq!(cfg(Mode::Debug, false).runtime_variant().unwrap(), RuntimeVariant::Debug);
        assert_eq!(cfg(Mode::Release, false).runtime_variant().unwrap(), RuntimeVariant::Release);
        // `opt-release` reuses the release archive: `-flto`/`-march=native` cannot be
        // prebuilt. See `runtime_variant`.
        assert_eq!(cfg(Mode::OptRelease, false).runtime_variant().unwrap(), RuntimeVariant::Release);
    }

    /// Any combination of the sanitizers the archive actually carries, however spelled.
    #[test]
    fn supported_sanitizers_select_the_instrumented_archive() {
        for req in [
            &["address"][..],
            &["undefined"][..],
            &["address", "undefined"][..],
            &["address,undefined"][..],
        ] {
            let got = with_sanitize(Mode::Debug, req).runtime_variant().unwrap();
            assert_eq!(got, RuntimeVariant::Sanitized, "for {req:?}");
        }
    }

    /// The rule that must not break: a sanitizer with no instrumented archive fails the
    /// build. Never a fallback to an uninstrumented runtime — a sanitizer reports nothing
    /// about code compiled without it, so the fallback would silently cover half the
    /// program.
    #[test]
    fn unsupported_sanitizers_are_an_error_not_a_fallback() {
        for req in [&["thread"][..], &["memory"][..], &["address", "thread"][..]] {
            let err = with_sanitize(Mode::Release, req)
                .runtime_variant()
                .expect_err("must refuse")
                .to_string();
            assert!(err.contains("thread") || err.contains("memory"), "{err}");
            assert!(err.contains("libneon_rt_san.a"), "{err}");
        }
    }

    /// The link must enable exactly the sanitizers the archive was compiled with. A
    /// proper subset does not link against it — `--sanitize address` alone left
    /// `__ubsan_handle_type_mismatch_v1` undefined, and `undefined` alone left
    /// `__asan_report_load4` undefined. `cli/tests/sanitizer_link.rs` proves this by
    /// actually linking; this pins the flag the fix rests on.
    #[test]
    fn the_link_enables_the_archives_full_sanitizer_set_not_the_subset_asked_for() {
        for req in [&["address"][..], &["undefined"][..], &["address,undefined"][..]] {
            let cfg = with_sanitize(Mode::Release, req);
            let args = cfg.cc_args(cfg.runtime_variant().unwrap());
            assert!(
                args.iter().any(|a| a == "-fsanitize=address,undefined"),
                "for {req:?} got {args:?}"
            );
            // Never a lone subset flag, which is precisely what failed to link.
            assert!(!args.iter().any(|a| a == "-fsanitize=address"), "for {req:?}");
            assert!(!args.iter().any(|a| a == "-fsanitize=undefined"), "for {req:?}");
        }
    }

    /// Widening is safe but must be visible, and must not fire when nothing was widened.
    #[test]
    fn widening_is_reported_only_when_it_happens() {
        let note = |req: &[&str]| {
            let cfg = with_sanitize(Mode::Release, req);
            cfg.sanitizer_widening_note(cfg.runtime_variant().unwrap())
        };
        assert!(note(&["address"]).unwrap().contains("undefined"));
        assert!(note(&["undefined"]).unwrap().contains("address"));
        // Asked for both, got both: nothing to say.
        assert!(note(&["address,undefined"]).is_none());
        assert!(note(&["address", "undefined"]).is_none());
        // No sanitizers at all: the release archive adds none.
        assert!(note(&[]).is_none());
    }

    /// A stacktrace needs walkable frames, and `opt-release` trims the frame pointer to
    /// get them back. The two are mutually exclusive; the trace wins where they meet.
    #[test]
    fn stacktrace_and_frame_pointer_omission_are_exclusive() {
        let args = |c: &BuildConfig| c.cc_args(c.runtime_variant().unwrap());
        let omit = |c: &BuildConfig| args(c).iter().any(|a| a == "-fomit-frame-pointer");
        let keep = |c: &BuildConfig| args(c).iter().any(|a| a == "-fno-omit-frame-pointer");

        // `opt-release` trims by default.
        assert!(omit(&cfg(Mode::OptRelease, false)));
        assert!(!keep(&cfg(Mode::OptRelease, false)));

        // Asking for traces suppresses the trim, and says so explicitly: `-O3` omits frame
        // pointers on most targets on its own, so dropping the flag would not be enough.
        assert!(!omit(&cfg(Mode::OptRelease, true)));
        assert!(keep(&cfg(Mode::OptRelease, true)));

        // The lighter modes never trimmed, but still state the requirement when asked.
        for mode in [Mode::Debug, Mode::Release] {
            assert!(!omit(&cfg(mode, true)));
            assert!(keep(&cfg(mode, true)));
            assert!(!keep(&cfg(mode, false)));
        }
    }
}
