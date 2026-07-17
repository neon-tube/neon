use crate::source;
use color_eyre::eyre::Result;
use neon_compiler::diagnostic::Renderer;
use neon_compiler::{lexer, parser};
use std::ffi::OsString;
use std::path::PathBuf;

pub fn run(file: &OsString) -> Result<()> {
    let path = PathBuf::from(file);
    let src = source::read(&path)?;
    let mut r = Renderer::for_stderr(&path, &src);

    let tokens = match lexer::lex(&src) {
        Ok(t) => t,
        Err(errors) => {
            report(&mut r, errors.iter().map(|e| (e.span.clone(), e.to_string())));
            std::process::exit(1);
        }
    };

    let (module, errors) = parser::parse(&tokens, src.len());
    if !errors.is_empty() {
        report(&mut r, errors.iter().map(|e| (e.span.clone(), e.to_string())));
        std::process::exit(1);
    }
    println!("{:#?}", module.expect("no errors means a module"));
    Ok(())
}

fn report(r: &mut Renderer, errors: impl Iterator<Item = (std::ops::Range<usize>, String)>) {
    for (span, msg) in errors {
        r.eprint(span, &msg);
    }
}
