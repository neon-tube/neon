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
    /// For the `file:line:` prefixes lowering bakes into panic messages: the user's
    /// file plus every stdlib module's, each by basename so no checkout path reaches a
    /// binary — and a stdlib `try!` reports the stdlib file, not a bogus user line.
    pub source: neon_compiler::ir::lower::SourceMap,
    pub module: ast::Module,
    /// The stdlib, by module path. Kept because its function *bodies* have to be lowered:
    /// the stdlib is real Neon code now, not only `@native` signatures.
    pub libs: Vec<(Vec<String>, ast::Module)>,
    /// The raw sources, kept so LOWERING errors (`Program::errors` — the `move` rules,
    /// which need reprs and so fire after checking) can render exactly like type errors.
    pub user_path: std::path::PathBuf,
    pub user_src: String,
    pub std_sources: Vec<(String, String)>,
}

/// How a non-root module's source is labelled in a diagnostic: a stdlib file behind its
/// `<stdlib>/` veil (its absolute path is a machine detail), a project module by where
/// it actually is.
fn shown_path(rel: &str) -> std::path::PathBuf {
    if rel.starts_with("std/") || rel == "prelude.neon" {
        std::path::PathBuf::from(format!("<stdlib>/{rel}"))
    } else {
        std::path::PathBuf::from(format!("src/{rel}"))
    }
}

/// Render a type error against the file its span actually indexes: the user's program
/// when the error's module is the root, the owning stdlib or project-module source
/// otherwise. Before this, a stdlib mistake rendered with the user's path and underlined
/// whatever token sat at that byte offset in the user's file.
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
            let shown = shown_path(rel);
            let mut r = Renderer::for_stderr(&shown, src);
            r.eprint_full(
                e.span.clone(),
                &e.to_string(),
                &e.labels(),
                e.help().as_deref(),
            );
        }
        None => {
            let mut r = Renderer::for_stderr(user_path, user_src);
            r.eprint_full(
                e.span.clone(),
                &e.to_string(),
                &e.labels(),
                e.help().as_deref(),
            );
        }
    }
}

/// Render the checker's warnings, each against the file its span indexes — the user's
/// program or the owning stdlib source, by the same rule as `eprint_type_error`. Never
/// exits: a warning is a note on a program that compiles.
pub fn eprint_warnings(
    warnings: &[neon_compiler::typecheck::result::Warning],
    user_path: &Path,
    user_src: &str,
    std_sources: &[(String, String)],
) {
    for w in warnings {
        let stdlib = (!w.module.is_empty())
            .then(|| {
                std_sources
                    .iter()
                    .find(|(rel, _)| neon_compiler::stdlib::module_path(rel) == w.module)
            })
            .flatten();
        match stdlib {
            Some((rel, wsrc)) => {
                let shown = shown_path(rel);
                let mut r = Renderer::for_stderr(&shown, wsrc);
                r.eprint_warning(w.span.clone(), &w.message);
            }
            None => {
                let mut r = Renderer::for_stderr(user_path, user_src);
                r.eprint_warning(w.span.clone(), &w.message);
            }
        }
    }
}

/// Type-check a source file, exiting with rendered diagnostics on any error.
/// `cfg` adds `@cfg` keys on top of the host's. A `@cfg`-guarded branch is dropped before
/// the checker, so without this the branch for another platform can be neither checked nor
/// built here — see `Config::for_host`.
pub fn check(path: &Path, lib: bool, cfg: &[String]) -> Result<Checked> {
    check_project(path, &[], lib, cfg)
}

/// Lex, parse and expand one source, rendering diagnostics against its own file and
/// exiting on any. The front half every module goes through, entry or extra.
fn parse_one(path: &Path, src: &str, config: &neon_compiler::expand::Config) -> ast::Module {
    let mut r = Renderer::for_stderr(path, src);
    let tokens = match lexer::lex(src) {
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
    let (module, _meta, expand_errors) = neon_compiler::expand::expand(module, config);
    if !expand_errors.is_empty() {
        for e in &expand_errors {
            r.eprint(e.span.clone(), &e.message);
        }
        std::process::exit(1);
    }
    module
}

/// Type-check a project: the entry at the root path plus `extras` — the other
/// `src/**/*.neon` files as `(src-relative path, absolute path)` — each becoming the
/// module its path names, by the same rule the stdlib's files follow. A lone file is a
/// project with no extras.
pub fn check_project(
    path: &Path,
    extras: &[(String, std::path::PathBuf)],
    lib: bool,
    cfg: &[String],
) -> Result<Checked> {
    let config = neon_compiler::expand::Config::for_host(cfg.iter().cloned());
    let src = source::read(path)?;
    let module = parse_one(path, &src, &config);

    // The other project files, each rendered against its own path on error.
    let mut extra_sources: Vec<(String, String)> = Vec::new();
    let mut extra_modules: Vec<ast::Module> = Vec::new();
    for (rel, abs) in extras {
        let esrc = source::read(abs)?;
        extra_modules.push(parse_one(abs, &esrc, &config));
        extra_sources.push((rel.clone(), esrc));
    }

    // The stdlib is numbered first, project modules after it, the entry last, so every
    // `ExprId` in the compilation is unique — one `TypecheckResult` covers all of it,
    // and every body can be checked and lowered like any other code.
    let std_sources = crate::stdlib::sources()?;
    let (std_modules, mut next_id) =
        neon_compiler::stdlib::parse_from_with(&std_sources, 0, &config)
            .map_err(|e| eyre!("{e}"))?;
    for m in &mut extra_modules {
        next_id = neon_compiler::ast::number_exprs_from(m, next_id);
    }
    let mut module = module;
    neon_compiler::ast::number_exprs_from(&mut module, next_id);

    // Project modules ride in `libs` beside the stdlib's: they are exactly that — modules
    // whose bodies get checked and lowered, distinguished only by who wrote them.
    let mut libs = std_modules;
    for ((rel, _), m) in extras.iter().zip(extra_modules) {
        libs.push((neon_compiler::stdlib::module_path(rel), m));
    }

    // One list for diagnostics: an error's module path finds its file here whether it
    // is the stdlib's or the project's.
    let mut all_sources = std_sources;
    all_sources.extend(extra_sources);

    let mut modules: Vec<(Vec<String>, &_)> = libs.iter().map(|(p, m)| (p.clone(), m)).collect();
    modules.push((Vec::new(), &module));

    let unit = if lib {
        Unit::Library
    } else {
        Unit::RootApplication
    };
    let mut env = Env::build_with(&modules, unit);
    if !env.errors().is_empty() {
        for e in env.errors() {
            eprint_type_error(e, path, &src, &all_sources);
        }
        std::process::exit(1);
    }
    // `check_all` returns every diagnostic, its own and the ones `Env::error` raises while
    // resolving annotations, sorted by span. There is no second list to consult.
    let (result, errs) = neon_compiler::typecheck::check::check_all(&mut env, &modules);
    if !errs.is_empty() {
        for e in &errs {
            eprint_type_error(e, path, &src, &all_sources);
        }
        std::process::exit(1);
    }
    // Warnings render like errors — against the owning source, stdlib, project module or
    // entry — and change nothing else: the compile continues.
    eprint_warnings(&result.warnings, path, &src, &all_sources);
    Ok(Checked {
        env,
        result,
        source: neon_compiler::ir::lower::SourceMap::new(
            neon_compiler::ir::lower::SourceInfo::new(&path.display().to_string(), &src),
            all_sources
                .iter()
                .map(|(rel, text)| {
                    (
                        neon_compiler::stdlib::module_path(rel),
                        neon_compiler::ir::lower::SourceInfo::new(rel, text),
                    )
                })
                .collect(),
        ),
        module,
        libs,
        user_path: path.to_path_buf(),
        user_src: src,
        std_sources: all_sources,
    })
}

/// Render lowering's own errors (`Program::errors`) and exit, exactly as `check` does for
/// the checker's. Called by every emit path right after `ir::compile*`.
pub fn exit_on_lower_errors(program: &neon_compiler::ir::ssa::Program, checked: &Checked) {
    if program.errors.is_empty() {
        return;
    }
    for e in &program.errors {
        eprint_type_error(e, &checked.user_path, &checked.user_src, &checked.std_sources);
    }
    std::process::exit(1);
}
