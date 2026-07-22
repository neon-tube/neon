//! Turning stdlib source into prefixed modules.
//!
//! Pure, per the filesystem rule: the CLI and the test harness read the files and
//! hand the text here. `stdlib/std/io.neon` becomes the module `std::io`, by path —
//! there is no `mod std { }` wrapper in the source.

use crate::ast::Module;
use crate::{lexer, parser};

/// The module prefix a stdlib-relative path denotes: `std/io.neon` → `["std","io"]`,
/// `std/collections/list.neon` → `["std","collections","list"]`.
pub fn module_path(rel: &str) -> Vec<String> {
    let rel = rel.strip_suffix(".neon").unwrap_or(rel);
    // The prelude gets a path of its own — one no source can write. Resolution consults
    // it *last*, after every scope the caller is in, which is what makes `Display`,
    // `Ordering` and the rest resolve by short name from anywhere while still letting a
    // program shadow any of them. It used to be declared at the root, `[]`, which is also
    // every program's own module path; see `Env::PRELUDE` for the four defects that one
    // collision caused.
    if rel == "prelude" {
        return vec![crate::typecheck::env::Env::PRELUDE.to_string()];
    }
    rel.split(['/', '\\']).filter(|s| !s.is_empty()).map(String::from).collect()
}

/// Parse `(relative-path, source)` pairs into prefixed modules.
///
/// A stdlib file that does not lex or parse is a broken toolchain, not a user error,
/// so it is an `Err` naming the file rather than a diagnostic.
pub fn parse(sources: &[(String, String)]) -> Result<Vec<(Vec<String>, Module)>, String> {
    parse_from(sources, 0).map(|(m, _)| m)
}

/// Parse the stdlib, numbering expressions from `base` so ids stay unique across the whole
/// compilation. Returns the modules and the next free id, which the program is numbered from.
pub fn parse_from(
    sources: &[(String, String)],
    base: u32,
) -> Result<(Vec<(Vec<String>, Module)>, u32), String> {
    let mut out = Vec::with_capacity(sources.len());
    let mut next = base;
    // The stdlib goes through `expand` like any user module — here, at the one
    // assembly point, so every consumer (the cli, the test harnesses) agrees. A
    // stdlib `@cfg` therefore works, and a typo'd stdlib annotation is a broken
    // toolchain rather than a silent no-op. Processors only keep or omit; the
    // annotations stay on the AST for the `@runtime`/`@pure` readers that consult it.
    let config = crate::expand::Config::with([
        std::env::consts::OS.to_string(),
        std::env::consts::ARCH.to_string(),
    ]);
    for (rel, src) in sources {
        let tokens = lexer::lex(src).map_err(|e| format!("stdlib `{rel}` did not lex: {e:?}"))?;
        let (module, errors) = parser::parse(&tokens, src.len());
        if !errors.is_empty() {
            return Err(format!("stdlib `{rel}` did not parse: {errors:?}"));
        }
        let module = module.ok_or_else(|| format!("stdlib `{rel}` produced no module"))?;
        let (mut module, _meta, expand_errors) = crate::expand::expand(module, &config);
        if !expand_errors.is_empty() {
            let shown: Vec<String> = expand_errors.iter().map(|e| e.message.clone()).collect();
            return Err(format!("stdlib `{rel}` did not expand: {}", shown.join("; ")));
        }
        next = crate::ast::number_exprs_from(&mut module, next);
        out.push((module_path(rel), module));
    }
    Ok((out, next))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_path_from_relative() {
        assert_eq!(module_path("std/io.neon"), vec!["std", "io"]);
        assert_eq!(module_path("std/collections/list.neon"), vec!["std", "collections", "list"]);
        // The prelude declares at a path of its own, which resolution consults last so
        // its short names need no `use` and a program can still shadow any of them. NOT
        // the root: that is the program's own path, and sharing it made prelude opaques
        // reachable from every program and prelude `use` re-exports unshadowable.
        assert_eq!(module_path("prelude.neon"), vec![crate::typecheck::env::Env::PRELUDE]);
        // Only the toolchain's own `prelude.neon` is special; a file that merely happens
        // to be named `prelude` inside a library is an ordinary module.
        assert_eq!(module_path("std/prelude.neon"), vec!["std", "prelude"]);
    }

    #[test]
    fn parses_a_native_signature() {
        let src = "@native(\"neon_io_println\") fn println(s: str)".to_string();
        let loaded = parse(&[("std/io.neon".to_string(), src)]).expect("parses");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].0, vec!["std", "io"]);
    }
}
