use crate::source;
use color_eyre::eyre::Result;
use neon_compiler::{lexer, parser};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub fn run(file: &OsString) -> Result<()> {
    let path = PathBuf::from(file);
    let src = source::read(&path)?;

    let tokens = match lexer::lex(&src) {
        Ok(t) => t,
        Err(errors) => {
            report(&path, &src, errors.iter().map(|e| (e.span.clone(), e.to_string())));
            std::process::exit(1);
        }
    };

    let (module, errors) = parser::parse(&tokens, src.len());
    if !errors.is_empty() {
        report(&path, &src, errors.iter().map(|e| (e.span.clone(), e.to_string())));
        std::process::exit(1);
    }
    println!("{:#?}", module.expect("no errors means a module"));
    Ok(())
}

fn report(path: &Path, src: &str, errors: impl Iterator<Item = (std::ops::Range<usize>, String)>) {
    for (span, msg) in errors {
        eprint!("{}", source::render(path, src, span, &msg));
    }
}
