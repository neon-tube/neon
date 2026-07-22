//! The shared front end: read a source file, run it through lexing, parsing, annotation
//! expansion, declaration building (with the stdlib) and type-checking. On any error it
//! renders diagnostics and exits; on success it hands back what codegen needs. Used by
//! every verb that has to type-check.

use crate::source;
use color_eyre::eyre::{eyre, Result};
use neon_compiler::diagnostic::Renderer;
use neon_compiler::typecheck::env::Unit;
use neon_compiler::typecheck::result::TypecheckResult;
use neon_compiler::typecheck::Env;
use neon_compiler::{ast, lexer, parser};
use std::path::Path;

/// A checked program, ready to lower.
pub struct Checked {
    pub env: Env,
    pub result: TypecheckResult,
    pub module: ast::Module,
    /// The stdlib, by module path. Kept because its function *bodies* have to be lowered:
    /// the stdlib is real Neon code now, not only `@native` signatures.
    pub libs: Vec<(Vec<String>, ast::Module)>,
}


/// Render a type error against the file its span actually indexes: the user's program
/// when the error's module is the root, the owning stdlib source otherwise. Before
/// this, a stdlib mistake rendered with the user's path and underlined whatever token
/// sat at that byte offset in the user's file.
pub fn eprint_type_error(
    e: &neon_compiler::typecheck::env::TypeError,
    user_path: &Path,
    user_src: &str,
    std_sources: &[(String, String)],
) {
    let stdlib = (!e.module.is_empty())
        .then(|| {
            std_sources
                .iter()
                .find(|(rel, _)| neon_compiler::stdlib::module_path(rel) == e.module)
        })
        .flatten();
    match stdlib {
        Some((rel, src)) => {
            let shown = std::path::PathBuf::from(format!("<stdlib>/{rel}"));
            let mut r = Renderer::for_stderr(&shown, src);
            r.eprint_full(e.span.clone(), &e.to_string(), &e.labels(), e.help().as_deref());
        }
        None => {
            let mut r = Renderer::for_stderr(user_path, user_src);
            r.eprint_full(e.span.clone(), &e.to_string(), &e.labels(), e.help().as_deref());
        }
    }
}

/// Type-check a source file, exiting with rendered diagnostics on any error.
pub fn check(path: &Path, lib: bool) -> Result<Checked> {
    let src = source::read(path)?;
    let mut r = Renderer::for_stderr(path, &src);

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

    // The stdlib is numbered first and the program after it, so every `ExprId` in the
    // compilation is unique — one `TypecheckResult` covers both, and stdlib bodies can be
    // checked and lowered like any other code.
    let std_sources = crate::stdlib::sources()?;
    let (std_modules, next_id) =
        neon_compiler::stdlib::parse_from(&std_sources, 0).map_err(|e| eyre!("{e}"))?;
    let mut module = module;
    neon_compiler::ast::number_exprs_from(&mut module, next_id);

    let mut modules: Vec<(Vec<String>, &_)> =
        std_modules.iter().map(|(p, m)| (p.clone(), m)).collect();
    modules.push((Vec::new(), &module));

    let unit = if lib { Unit::Library } else { Unit::RootApplication };
    let mut env = Env::build_with(&modules, unit);
    if !env.errors().is_empty() {
        for e in env.errors() {
            eprint_type_error(e, path, &src, &std_sources);
        }
        std::process::exit(1);
    }
    // `check_all` returns every diagnostic, its own and the ones `Env::error` raises while
    // resolving annotations, sorted by span. There is no second list to consult.
    let (result, errs) = neon_compiler::typecheck::check::check_all(&mut env, &modules);
    if !errs.is_empty() {
        for e in &errs {
            eprint_type_error(e, path, &src, &std_sources);
        }
        std::process::exit(1);
    }
    Ok(Checked { env, result, module, libs: std_modules })
}
