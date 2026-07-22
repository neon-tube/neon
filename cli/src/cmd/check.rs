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

    // Annotation expansion: `@cfg` drops code the target does not want, `@doc` is pulled
    // aside, `@native` is validated, and an unknown `@name` is an error. Seed `@cfg` with
    // the host's OS and arch (host == target until cross-compilation exists).
    let config = neon_compiler::expand::Config::with([
        std::env::consts::OS.to_string(),
        std::env::consts::ARCH.to_string(),
    ]);
    let (module, _meta, expand_errors) = neon_compiler::expand::expand(module, &config);
    if !expand_errors.is_empty() {
        for e in &expand_errors {
            r.eprint(e.span.clone(), &e.message);
        }
        std::process::exit(1);
    }

    let unit = if lib { Unit::Library } else { Unit::RootApplication };

    // The stdlib is declared alongside the program, so `use std::io` resolves.
    let std_sources = crate::stdlib::sources()?;
    let std_modules = neon_compiler::stdlib::parse(&std_sources)
        .map_err(|e| color_eyre::eyre::eyre!("{e}"))?;
    let mut modules: Vec<(Vec<String>, &_)> =
        std_modules.iter().map(|(p, m)| (p.clone(), m)).collect();
    modules.push((Vec::new(), &module));

    let mut env = Env::build_with(&modules, unit);
    // Declarations first: a body checked against a signature that did not resolve
    // would report the same mistake twice. When they are sound, `check_module` returns
    // every diagnostic of the run — its own and the ones raised while resolving
    // annotations — so there is one list either way.
    let errors = if env.errors().is_empty() {
        neon_compiler::typecheck::check::check_module(&mut env, &module).1
    } else {
        env.take_errors()
    };
    if !errors.is_empty() {
        for e in &errors {
            crate::frontend::eprint_type_error(e, &path, &src, &std_sources);
        }
        let n = errors.len();
        eprintln!("{n} error{}", if n == 1 { "" } else { "s" });
        std::process::exit(1);
    }
    Ok(())
}
