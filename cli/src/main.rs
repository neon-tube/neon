mod cmd;
mod source;
mod sysroot;

use clap::{Parser, Subcommand};
use color_eyre::eyre::Result;
use std::ffi::OsString;

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
    Check { file: OsString },
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
    /// Print the resolved sysroot.
    Sysroot,
}

fn main() -> Result<()> {
    color_eyre::install()?;
    match Cli::parse().command {
        Command::Lex { file, spans } => cmd::lex::run(&file, spans),
        Command::Parse { file } => cmd::parse::run(&file),
        Command::Check { file } => cmd::check::run(&file),
        Command::Fmt { file, write, check } => cmd::fmt::run(&file, write, check),
        Command::Sysroot => cmd::sysroot::run(),
    }
}
