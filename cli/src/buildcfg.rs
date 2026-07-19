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

    /// The full argument list for the C compiler (excluding sources and `-o`, which the
    /// caller supplies).
    pub fn cc_args(&self) -> Vec<String> {
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
        if !self.mode.is_debug() {
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

        for s in &self.sanitize {
            args.push(format!("-fsanitize={s}"));
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

    /// A stacktrace needs walkable frames, and `opt-release` trims the frame pointer to
    /// get them back. The two are mutually exclusive; the trace wins where they meet.
    #[test]
    fn stacktrace_and_frame_pointer_omission_are_exclusive() {
        let omit = |c: &BuildConfig| c.cc_args().iter().any(|a| a == "-fomit-frame-pointer");
        let keep = |c: &BuildConfig| c.cc_args().iter().any(|a| a == "-fno-omit-frame-pointer");

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
