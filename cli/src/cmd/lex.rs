use crate::source;
use color_eyre::eyre::Result;
use neon_compiler::lexer;
use std::ffi::OsString;
use std::path::PathBuf;

pub fn run(file: &OsString, spans: bool) -> Result<()> {
    let path = PathBuf::from(file);
    let src = source::read(&path)?;

    match lexer::lex(&src) {
        Ok(tokens) => {
            for t in tokens {
                if spans {
                    println!("{:>5}..{:<5} {:?}", t.span.start, t.span.end, t.token);
                } else {
                    println!("{:?}", t.token);
                }
            }
            Ok(())
        }
        Err(errors) => {
            // Every error, not just the first: the lexer accumulates so a
            // diagnostics pass can show them all.
            for e in &errors {
                eprintln!(
                    "{}:{}: error: {}",
                    path.display(),
                    source::line_of(&src, e.span.start),
                    e
                );
            }
            std::process::exit(1);
        }
    }
}
