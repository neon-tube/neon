use crate::source;
use color_eyre::eyre::{bail, eyre, Result};
use neon_compiler::backend::c;
use neon_compiler::diagnostic::Renderer;
use neon_compiler::ir::{self, Stage};
use neon_compiler::typecheck::env::Unit;
use neon_compiler::typecheck::Env;
use neon_compiler::{lexer, parser};
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

/// Compile a source file to an executable: run the pipeline, emit C, and hand it to `cc`
/// along with the runtime.
pub fn run(file: &OsString, output: Option<OsString>) -> Result<()> {
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

    let std_sources = crate::stdlib::sources()?;
    let std_modules =
        neon_compiler::stdlib::parse(&std_sources).map_err(|e| eyre!("{e}"))?;
    let mut modules: Vec<(Vec<String>, &_)> =
        std_modules.iter().map(|(p, m)| (p.clone(), m)).collect();
    modules.push((Vec::new(), &module));

    let mut env = Env::build_with(&modules, Unit::RootApplication);
    if !env.errors().is_empty() {
        for e in env.errors() {
            r.eprint_full(e.span.clone(), &e.to_string(), &e.labels(), e.help().as_deref());
        }
        std::process::exit(1);
    }
    let (result, errs) = neon_compiler::typecheck::check::check_module(&mut env, &module);
    if !errs.is_empty() {
        for e in &errs {
            r.eprint_full(e.span.clone(), &e.to_string(), &e.labels(), e.help().as_deref());
        }
        std::process::exit(1);
    }

    // Front end clean: lower, optimise, refcount, and emit C.
    let program = ir::compile(&env, &result, &module, Stage::Final);
    let c_source = c::emit(&program);

    // The output executable, and a sibling `.c` we hand to the compiler.
    let out = output.map(PathBuf::from).unwrap_or_else(|| path.with_extension(""));
    let c_file = out.with_extension("c");
    std::fs::write(&c_file, &c_source).map_err(|e| eyre!("writing {}: {e}", c_file.display()))?;

    let (include, rt_c) = runtime_sources()?;
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let status = Command::new(&cc)
        .arg("-std=c11")
        .arg("-O2")
        .arg("-o")
        .arg(&out)
        .arg(&c_file)
        .arg(&rt_c)
        .arg("-I")
        .arg(&include)
        .status()
        .map_err(|e| eyre!("could not run the C compiler `{cc}`: {e}"))?;
    if !status.success() {
        bail!("the C compiler failed on {}", c_file.display());
    }
    Ok(())
}

/// The runtime's include directory and `rt.c`, found under the sysroot.
fn runtime_sources() -> Result<(PathBuf, PathBuf)> {
    let root = match std::env::var_os("NEON_SYSROOT") {
        Some(dir) => PathBuf::from(dir).join("runtime"),
        None => {
            let exe = std::env::current_exe()?;
            exe.parent()
                .ok_or_else(|| eyre!("the neon binary has no parent directory"))?
                .join("../runtime")
        }
    };
    let include = root.join("include");
    let rt_c = root.join("src/rt.c");
    if !rt_c.is_file() {
        bail!("cannot find the runtime at {}", root.display());
    }
    Ok((include, rt_c))
}
