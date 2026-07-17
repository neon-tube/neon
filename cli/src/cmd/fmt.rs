use crate::source;
use color_eyre::eyre::{Context, Result};
use neon_compiler::diagnostic::Renderer;
use neon_compiler::format::{self, FormatError};
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn run(file: &OsString, write: bool, check: bool) -> Result<()> {
    let path = PathBuf::from(file);
    let src = source::read(&path)?;

    let formatted = match format::format(&src) {
        Ok(s) => s,
        Err(e) => {
            report(&path, &src, e);
            std::process::exit(1);
        }
    };

    if check {
        // Nothing on stdout: the exit code is the answer.
        if formatted != src {
            std::process::exit(1);
        }
        return Ok(());
    }

    if write {
        // Only when it differs, so a formatted tree keeps its mtimes and
        // whatever is watching it stays quiet.
        if formatted != src {
            std::fs::write(&path, formatted)
                .wrap_err_with(|| format!("cannot write '{}'", path.display()))?;
        }
        return Ok(());
    }

    // print!, not println!: the formatter's output already ends in a newline.
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(formatted.as_bytes())?;
    Ok(())
}

fn report(path: &Path, src: &str, e: FormatError) {
    let errors: Vec<(std::ops::Range<usize>, String)> = match e {
        FormatError::Lex(es) => es.iter().map(|e| (e.span.clone(), e.to_string())).collect(),
        FormatError::Parse(es) => es.iter().map(|e| (e.span.clone(), e.to_string())).collect(),
    };
    let mut r = Renderer::for_stderr(path, src);
    for (span, msg) in errors {
        r.eprint(span, &msg);
    }
}
