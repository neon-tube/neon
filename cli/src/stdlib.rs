//! Reading the on-disk stdlib into source pairs.
//!
//! The fs half of loading: the compiler's `stdlib::parse` is pure and takes the
//! text, so this is the only place that walks the directory. Each `.neon` becomes
//! `(path-relative-to-root, source)`, and the compiler turns the path into a module
//! prefix — `std/io.neon` → `std::io`.

use crate::sysroot::Sysroot;
use color_eyre::eyre::{Context, Result};
use std::path::Path;

/// Every stdlib file as `(relative path, source)`, sorted for a stable module order.
pub fn sources() -> Result<Vec<(String, String)>> {
    sources_from(&Sysroot::stdlib_dir()?)
}

/// The walk, against an explicit root. Separate so a test can point at the repo's
/// stdlib without going through the install-layout probe.
pub fn sources_from(root: &Path) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    collect(root, root, &mut out)?;
    out.sort();
    Ok(out)
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) -> Result<()> {
    let entries =
        std::fs::read_dir(dir).wrap_err_with(|| format!("reading '{}'", dir.display()))?;
    for entry in entries {
        let path = entry?.path();
        if path.is_dir() {
            collect(root, &path, out)?;
        } else if path.extension().is_some_and(|e| e == "neon") {
            let rel = path
                .strip_prefix(root)
                .expect("collected under root")
                .to_string_lossy()
                .replace('\\', "/");
            let src = std::fs::read_to_string(&path)
                .wrap_err_with(|| format!("reading '{}'", path.display()))?;
            out.push((rel, src));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use neon_compiler::typecheck::env::Unit;
    use neon_compiler::typecheck::{check, Env};
    use neon_compiler::{lexer, parser, stdlib};

    /// The on-disk stdlib loads and a program resolves `io::println` against it —
    /// the whole Phase 1 path, from files to a clean check, without a runtime.
    #[test]
    fn a_program_checks_against_the_on_disk_stdlib() {
        // The repo stdlib directly, not via the install-layout probe.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../stdlib");
        let sources = sources_from(&root).expect("stdlib on disk");
        assert!(
            sources.iter().any(|(rel, _)| rel == "std/io.neon"),
            "io.neon is present"
        );

        let std_modules = stdlib::parse(&sources).expect("stdlib parses");

        let src = "use std::io\nfn main() { io::println(\"hi\") }";
        let tokens = lexer::lex(src).expect("lexes");
        let (user, errs) = parser::parse(&tokens, src.len());
        assert!(errs.is_empty(), "{errs:?}");
        let user = user.expect("parses");

        let mut modules: Vec<(Vec<String>, &_)> =
            std_modules.iter().map(|(p, m)| (p.clone(), m)).collect();
        modules.push((Vec::new(), &user));

        let mut env = Env::build_with(&modules, Unit::RootApplication);
        assert!(env.errors().is_empty(), "declarations: {:?}", env.errors());
        let (_r, errs) = check::check_module(&mut env, &user);
        assert!(errs.is_empty(), "io::println should resolve: {errs:?}");
    }
}
