use crate::source;
use color_eyre::eyre::Result;
use neon_compiler::diagnostic::Renderer;
use neon_compiler::typecheck::env::Unit;
use neon_compiler::typecheck::Env;
use neon_compiler::{lexer, parser};
use std::ffi::OsString;
use std::path::PathBuf;

pub fn run(file: &OsString, lib: bool) -> Result<()> {
    let path = PathBuf::from(file);
    let src = source::read(&path)?;
    let mut r = Renderer::for_stderr(&path, &src);

    let tokens = match lexer::lex(&src) {
        Ok(t) => t,
        Err(errors) => {
            for e in &errors {
                r.eprint(e.span.clone(), &e.to_string());
            }
            std::process::exit(1);
        }
    };

    let (module, errors) = parser::parse(&tokens, src.len());
    if !errors.is_empty() {
        for e in &errors {
            r.eprint(e.span.clone(), &e.to_string());
        }
        std::process::exit(1);
    }
    let module = module.expect("no errors means a module");

    let unit = if lib { Unit::Library } else { Unit::RootApplication };
    let mut env = Env::build_as(&module, unit);
    // Declarations first: a body checked against a signature that did not resolve
    // would report the same mistake twice.
    if env.errors().is_empty() {
        let (_result, errs) = neon_compiler::typecheck::check::check_module(&mut env, &module);
        env.extend_errors(errs);
    }
    let errors = env.errors();
    if !errors.is_empty() {
        for e in errors {
            r.eprint(e.span.clone(), &e.to_string());
        }
        let n = errors.len();
        eprintln!("{n} error{}", if n == 1 { "" } else { "s" });
        std::process::exit(1);
    }
    Ok(())
}
