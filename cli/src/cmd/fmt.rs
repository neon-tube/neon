use crate::source;
use color_eyre::eyre::{Context, Result};
use neon_compiler::diagnostic::Renderer;
use neon_compiler::format::{self, FormatError};
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn run(file: Option<&OsString>, write: bool, check: bool) -> Result<()> {
    let Some(file) = file else {
        return run_project(write, check);
    };
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

/// `neon fmt` with no file: the whole project. Requires `--write` or `--check` —
/// concatenating every formatted file to stdout answers no question anyone asks, and
/// erroring beats guessing which one was meant.
fn run_project(write: bool, check: bool) -> Result<()> {
    if !write && !check {
        eprintln!("neon fmt with no file formats the whole project; pass --write to apply or --check to verify");
        std::process::exit(2);
    }
    let project = crate::project::Project::find(Path::new("."))?;
    let mut files = Vec::new();
    collect_neon(&project.root.join("src"), &mut files);
    files.sort();

    let mut dirty = Vec::new();
    for path in &files {
        let src = source::read(path)?;
        let formatted = match format::format(&src) {
            Ok(s) => s,
            Err(e) => {
                report(path, &src, e);
                std::process::exit(1);
            }
        };
        if formatted == src {
            continue;
        }
        if write {
            std::fs::write(path, formatted)
                .wrap_err_with(|| format!("cannot write '{}'", path.display()))?;
        }
        dirty.push(path);
    }
    if check && !dirty.is_empty() {
        for p in &dirty {
            eprintln!("not formatted: {}", p.display());
        }
        std::process::exit(1);
    }
    if write {
        eprintln!(
            "formatted {} file{}, {} changed",
            files.len(),
            if files.len() == 1 { "" } else { "s" },
            dirty.len()
        );
    }
    Ok(())
}

fn collect_neon(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_neon(&p, out);
        } else if p.extension().is_some_and(|x| x == "neon") {
            out.push(p);
        }
    }
}
