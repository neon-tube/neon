use crate::source;
use color_eyre::eyre::Result;
use neon_compiler::typecheck::Env;
use neon_compiler::{lexer, parser};
use std::ffi::OsString;
use std::path::PathBuf;

pub fn run(file: &OsString) -> Result<()> {
    let path = PathBuf::from(file);
    let src = source::read(&path)?;

    let tokens = match lexer::lex(&src) {
        Ok(t) => t,
        Err(errors) => {
            for e in &errors {
                eprint!("{}", source::render(&path, &src, e.span.clone(), &e.to_string()));
            }
            std::process::exit(1);
        }
    };

    let (module, errors) = parser::parse(&tokens, src.len());
    if !errors.is_empty() {
        for e in &errors {
            eprint!("{}", source::render(&path, &src, e.span.clone(), &e.to_string()));
        }
        std::process::exit(1);
    }
    let module = module.expect("no errors means a module");

    let env = Env::build(&module);
    let errors = env.errors();
    if !errors.is_empty() {
        for e in errors {
            eprint!("{}", source::render(&path, &src, e.span.clone(), &e.to_string()));
        }
        let n = errors.len();
        eprintln!("{n} error{}", if n == 1 { "" } else { "s" });
        std::process::exit(1);
    }
    Ok(())
}
