mod buildcfg;
mod cmd;
mod emit;
mod frontend;
mod project;
mod source;
mod stdlib;
mod sysroot;

use buildcfg::{Allocator, BuildFlags, Mode};
use clap::{Args, Parser, Subcommand, ValueEnum};
use color_eyre::eyre::Result;
use std::ffi::OsString;
use std::path::PathBuf;

/// Flags shared by every verb that drives the C compiler. Layered over `neon.toml`'s
/// `[build]` table, which is layered over built-in defaults.
#[derive(Args)]
struct BuildOpts {
    /// The C compiler to invoke (defaults to `$CC` or `cc`).
    #[arg(long)]
    cc: Option<String>,
    /// The build preset: `debug` (-O0 + symbols + assertions), `release` (-O3), or
    /// `opt-release` (-O3 + LTO + native + trimmings). Defaults to `release`.
    #[arg(long, value_enum)]
    mode: Option<Mode>,
    /// Override the optimisation level as `-O<level>`, regardless of mode.
    #[arg(short = 'O', long)]
    opt: Option<String>,
    /// Emit debug symbols (`-g`). Always on in debug mode.
    #[arg(short = 'g', long)]
    debug_symbols: bool,
    /// A sanitizer to enable, e.g. `address` or `undefined` (repeatable).
    #[arg(long)]
    sanitize: Vec<String>,
    /// Swap the memory allocator.
    #[arg(long, value_enum)]
    allocator: Option<Allocator>,
    /// Keep frames walkable so a `throw` can capture a stacktrace. Overrides
    /// `opt-release`'s frame-pointer trimming, which would otherwise make frames
    /// unwalkable. Also settable as `stacktrace` in `neon.toml`'s `[build]`.
    #[arg(long)]
    stacktrace: bool,
    /// A raw flag passed straight through to the C compiler (repeatable). Values almost
    /// always begin with `-`, so hyphen-led values are taken literally.
    #[arg(short = 'C', long = "cflag", allow_hyphen_values = true)]
    cflag: Vec<String>,
    /// An extra `@cfg` key, on top of the host's OS and arch (repeatable).
    ///
    /// A `@cfg`-guarded branch is dropped before the checker runs, so without this the
    /// branch for another platform can be neither checked nor built here. Pair it with
    /// `--cc` and `--runtime` to actually produce a binary for that platform:
    /// `--cfg windows --cc x86_64-w64-mingw32-gcc --runtime <mingw archive>`.
    #[arg(long = "cfg", value_name = "KEY")]
    cfg: Vec<String>,
    /// The runtime archive to link, instead of the one the sysroot picks for this host.
    ///
    /// The sysroot chooses by `cc` FLAVOUR (gcc or clang) for the host, which is the right
    /// answer for a host build and has no answer at all for a cross build. Cross-compiling
    /// needs an archive built by the cross toolchain, and this is how it is named.
    #[arg(long, value_name = "ARCHIVE")]
    runtime: Option<PathBuf>,
}

impl From<BuildOpts> for BuildFlags {
    fn from(o: BuildOpts) -> Self {
        BuildFlags {
            cc: o.cc,
            mode: o.mode,
            opt: o.opt,
            // Only a present `-g` overrides the layer below; its absence leaves it alone.
            debug_symbols: o.debug_symbols.then_some(true),
            sanitize: o.sanitize,
            allocator: o.allocator,
            // Absence leaves the layer below alone, like `-g`.
            stacktrace: o.stacktrace.then_some(true),
            cfg: o.cfg,
            runtime: o.runtime,
            cflags: o.cflag,
        }
    }
}

/// Which pipeline stage `neon ir` prints.
#[derive(Clone, Copy, ValueEnum)]
enum IrStage {
    /// Straight out of lowering and monomorphisation, before any pass.
    Lowered,
    /// After the optimiser.
    Opt,
    /// After refcount insertion -- the IR that would be emitted.
    Final,
}

impl From<IrStage> for neon_compiler::ir::Stage {
    fn from(s: IrStage) -> Self {
        match s {
            IrStage::Lowered => neon_compiler::ir::Stage::Lowered,
            IrStage::Opt => neon_compiler::ir::Stage::Optimised,
            IrStage::Final => neon_compiler::ir::Stage::Final,
        }
    }
}

#[derive(Parser)]
#[command(name = "neon", version, about = "The Neon toolchain")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Lex a source file and print its tokens.
    Lex {
        /// OsString, not PathBuf-from-String: a path need not be UTF-8, and
        /// rejecting one at the arg parser is a worse error than failing to
        /// open it.
        file: OsString,
        /// Print byte spans alongside each token.
        #[arg(long)]
        spans: bool,
    },
    /// Parse a source file and print its syntax tree.
    Parse { file: OsString },
    /// Type-check a source file, or the whole project when no file is named. Prints
    /// nothing and exits 0 when it is well typed.
    Check {
        /// A `.neon` file; absent means the current project — the entry plus every
        /// module under `src/`.
        file: Option<OsString>,
        /// Check as something other programs may depend on, rather than as the
        /// root application. An `orphan impl` is rejected here: a library
        /// carrying one imposes its choice on every dependent.
        #[arg(long)]
        lib: bool,
        /// An extra `@cfg` key, on top of the host's OS and arch. Repeatable.
        ///
        /// A `@cfg`-guarded branch is dropped before the checker runs, so the branch for
        /// another platform is never type-checked on this one. This is how it gets
        /// checked: `neon check --cfg windows` compiles the Windows half. It ADDS keys
        /// rather than describing a target -- nothing here cross-compiles.
        #[arg(long = "cfg", value_name = "KEY")]
        cfg: Vec<String>,
    },
    /// Format a source file, or the whole project when no file is named.
    Fmt {
        /// A `.neon` file; absent means every file under the project's `src/`
        /// (which then requires `--write` or `--check`).
        file: Option<OsString>,
        /// Write the result back to the file instead of printing it.
        #[arg(long, conflicts_with = "check")]
        write: bool,
        /// Print nothing; exit 1 if the file is not already formatted.
        #[arg(long)]
        check: bool,
    },
    /// Emit the intermediate representation for a source file.
    Ir {
        file: OsString,
        /// Which pipeline stage to print. Defaults to the final, emit-ready IR.
        #[arg(long, value_enum, default_value_t = IrStage::Final)]
        stage: IrStage,
    },
    /// Create a new project: a `neon.toml` and a `src/main.neon`.
    Init {
        /// The project directory to create (defaults to the working directory).
        name: Option<OsString>,
        /// Also write an `AGENTS.md`: a guide to writing Neon for a coding agent that
        /// has never seen the language, covering its idioms and the gotchas that trip
        /// up an assumption carried over from another language.
        #[arg(long)]
        agent: bool,
    },
    /// Compile a single source file to an executable.
    Compile {
        file: OsString,
        /// The output executable (defaults to the source name without its extension).
        #[arg(short)]
        output: Option<OsString>,
        #[command(flatten)]
        build: BuildOpts,
    },
    /// Build the project containing the working directory into `target/`.
    Build {
        #[command(flatten)]
        build: BuildOpts,
    },
    /// Build and run a project or a single `.neon` file.
    Run {
        /// A `.neon` file, a project directory, or nothing for the current project.
        path: Option<OsString>,
        /// Re-run on every source change.
        #[arg(long)]
        watch: bool,
        #[command(flatten)]
        build: BuildOpts,
        /// Arguments forwarded to the program, after `--`.
        #[arg(last = true)]
        args: Vec<OsString>,
    },
    /// Time `bench` blocks: warm up, run batches until stable, report ns per iteration.
    Bench {
        /// A `.neon` file; absent means the current project — the entry plus every
        /// module under `src/`.
        file: Option<OsString>,
        /// Run only the benches whose name contains this substring.
        #[arg(long)]
        filter: Option<String>,
        #[command(flatten)]
        build: BuildOpts,
    },
    /// Run `test` blocks, one per line of output, and exit non-zero if any failed.
    Test {
        /// A `.neon` file; absent means the current project — the entry plus every
        /// module under `src/`, so tests live next to the code they test.
        file: Option<OsString>,
        /// Re-run on every source change.
        #[arg(long)]
        watch: bool,
        /// Run only the tests whose name contains this substring.
        #[arg(long)]
        filter: Option<String>,
        #[command(flatten)]
        build: BuildOpts,
    },
    /// Show a module's documentation: signatures and `///` docs, from the source.
    Doc {
        /// A stdlib module (`std::io`), a `.neon` file, or nothing for the module index.
        target: Option<OsString>,
    },
    /// Print the resolved sysroot.
    Sysroot {
        /// Print only the stdlib directory, as one bare path. For tools, not people —
        /// this is how `neon-lsp` asks the toolchain where its stdlib is.
        #[arg(long)]
        stdlib: bool,
    },
    /// Diagnose the toolchain setup: sysroot, runtime archives, compilers, and an
    /// end-to-end compile-and-run smoke test. Exits non-zero on hard failures.
    Doctor,
}

/// Present an internal panic as what it is: a compiler bug, not a problem with the
/// user's program. The compiler's own invariant violations deliberately panic with
/// `internal error: ...` messages rather than mis-compiling (see `ir/lower.rs`); this
/// hook is the user-facing half — without it they surface as a raw Rust backtrace that
/// reads like the user did something wrong. The process still exits non-zero through
/// the normal unwind.
fn install_ice_hook() {
    std::panic::set_hook(Box::new(|info| {
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .map(str::to_string)
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<no message>".to_string());
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        eprintln!("internal compiler error: this is a bug in neon, not in your program.");
        eprintln!("  {msg}");
        eprintln!("  at {location}, neon {}", neon_compiler::version());
        eprintln!("  please report it with the source file that triggered it.");
        if std::env::var_os("RUST_BACKTRACE").is_some() {
            eprintln!("{}", std::backtrace::Backtrace::force_capture());
        } else {
            eprintln!("  (set RUST_BACKTRACE=1 for a backtrace)");
        }
    }));
}

fn main() -> Result<()> {
    color_eyre::install()?;
    install_ice_hook();
    match Cli::parse().command {
        Command::Lex { file, spans } => cmd::lex::run(&file, spans),
        Command::Parse { file } => cmd::parse::run(&file),
        Command::Check { file, lib, cfg } => cmd::check::run(file.as_ref(), lib, &cfg),
        Command::Fmt { file, write, check } => cmd::fmt::run(file.as_ref(), write, check),
        Command::Ir { file, stage } => cmd::ir::run(&file, stage.into()),
        Command::Init { name, agent } => cmd::init::run(name, agent),
        Command::Compile {
            file,
            output,
            build,
        } => cmd::compile::run(&file, output, build.into()),
        Command::Build { build } => cmd::build::run(build.into()),
        Command::Run {
            path,
            watch,
            build,
            args,
        } => {
            if watch {
                return cmd::watch::rerun_on_change(cmd::watch::watch_set(path.as_ref()));
            }
            cmd::run::run(path, args, build.into())
        }
        Command::Test {
            file,
            watch,
            filter,
            build,
        } => {
            if watch {
                return cmd::watch::rerun_on_change(cmd::watch::watch_set(file.as_ref()));
            }
            cmd::test::run(file.as_ref(), filter, build.into())
        }
        Command::Bench {
            file,
            filter,
            build,
        } => cmd::bench::run(file.as_ref(), filter, build.into()),
        Command::Doc { target } => cmd::doc::run(target.as_ref()),
        Command::Sysroot { stdlib } => cmd::sysroot::run(stdlib),
        Command::Doctor => cmd::doctor::run(),
    }
}
