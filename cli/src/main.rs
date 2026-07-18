mod cmd;
mod source;
mod stdlib;
mod sysroot;

use clap::{Parser, Subcommand, ValueEnum};
use color_eyre::eyre::Result;
use std::ffi::OsString;

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
    /// Type-check a source file. Prints nothing and exits 0 when it is well typed.
    Check {
        file: OsString,
        /// Check as something other programs may depend on, rather than as the
        /// root application. An `orphan impl` is rejected here: a library
        /// carrying one imposes its choice on every dependent.
        #[arg(long)]
        lib: bool,
    },
    /// Format a source file. Prints the result to stdout by default.
    Fmt {
        file: OsString,
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
    /// Compile a source file to an executable.
    Compile {
        file: OsString,
        /// The output executable (defaults to the source name without its extension).
        #[arg(short)]
        output: Option<OsString>,
    },
    /// Print the resolved sysroot.
    Sysroot,
}

fn main() -> Result<()> {
    color_eyre::install()?;
    match Cli::parse().command {
        Command::Lex { file, spans } => cmd::lex::run(&file, spans),
        Command::Parse { file } => cmd::parse::run(&file),
        Command::Check { file, lib } => cmd::check::run(&file, lib),
        Command::Fmt { file, write, check } => cmd::fmt::run(&file, write, check),
        Command::Ir { file, stage } => cmd::ir::run(&file, stage.into()),
        Command::Compile { file, output } => cmd::compile::run(&file, output),
        Command::Sysroot => cmd::sysroot::run(),
    }
}
