//! `neon doc`: what a module offers, from the one place that cannot drift — its source.
//!
//! Signatures are SLICED out of the file rather than reconstructed from the AST: a
//! printer would be a second rendering of the type grammar that has to be kept in step
//! with the parser forever, while the source is already exactly what the author wrote.
//! Docs are the `///` runs the lexer keeps as trivia, recovered by the same walk the
//! language server's hover uses (`lexer::doc_above`), so the two can never disagree
//! about what a declaration documents itself as.
//!
//! `neon doc std::io` documents a stdlib module; `neon doc src/main.neon` a file; bare
//! `neon doc` lists the stdlib modules. `internal` modules are skipped — the fence
//! exists so outsiders do not build against them, and an index is how outsiders find
//! things.

use color_eyre::eyre::{eyre, Result};
use neon_compiler::ast::{Decl, DeclKind};
use neon_compiler::{lexer, parser, stdlib};
use std::ffi::OsString;
use std::path::PathBuf;

pub fn run(target: Option<&OsString>) -> Result<()> {
    let std_sources = crate::stdlib::sources()?;
    let Some(target) = target else {
        // No argument: the index. One line per module, first doc line as its blurb.
        println!("stdlib modules:");
        for (rel, text) in &std_sources {
            let module = stdlib::module_path(rel);
            if module.first().map(String::as_str) == Some("#prelude") {
                continue;
            }
            let blurb = first_module_doc(text).unwrap_or_default();
            println!("  {:<24} {blurb}", module.join("::"));
        }
        println!("\nneon doc <module> shows a module; neon doc <file.neon> a file.");
        return Ok(());
    };

    let t = target.to_string_lossy();
    let (name, text) = if t.ends_with(".neon") {
        let path = PathBuf::from(target);
        let text = crate::source::read(&path)?;
        (t.to_string(), text)
    } else {
        let (rel, text) = std_sources
            .iter()
            .find(|(rel, _)| stdlib::module_path(rel).join("::") == t)
            .ok_or_else(|| eyre!("no stdlib module named `{t}`; run `neon doc` for the list"))?;
        (stdlib::module_path(rel).join("::"), text.clone())
    };

    let lexed = lexer::lex_full(&text).map_err(|e| eyre!("{}: did not lex: {e:?}", name))?;
    let (module, errors) = parser::parse(&lexed.tokens, text.len());
    if !errors.is_empty() {
        return Err(eyre!(
            "{name}: did not parse; run `neon check` for the diagnostics"
        ));
    }
    let mut module = module.ok_or_else(|| eyre!("{name}: no module"))?;
    // Attachment happens once, by the grammar's one adjacency rule; everything below
    // just reads the field.
    neon_compiler::ast::attach_docs(&mut module, &text, &lexed.trivia);

    println!("{name}\n");
    for d in &module.decls {
        print_decl(&text, d, 0);
    }
    Ok(())
}

/// The comment at the very top of a file — its module documentation — used as the
/// index blurb. Only the first line: an index row is a pointer, not the manual. A
/// plain `//` header counts here, deliberately: the stdlib documents its modules that
/// way today, and an index with an empty blurb column helps nobody.
fn first_module_doc(text: &str) -> Option<String> {
    let lexed = lexer::lex_full(text).ok()?;
    let first = lexed.trivia.first()?;
    (first.span.start == 0).then(|| first.text.trim().to_string())
}

fn print_decl(text: &str, d: &Decl, indent: usize) {
    let pad = "    ".repeat(indent);
    match &d.kind {
        // Imports are plumbing, not interface.
        DeclKind::Use(_) => {}
        DeclKind::Mod(m) => {
            if m.internal {
                return;
            }
            print_doc(d, &pad);
            println!("{pad}mod {} {{", m.name);
            for inner in &m.decls {
                print_decl(text, inner, indent + 1);
            }
            println!("{pad}}}\n");
        }
        // A fn's interface is its signature: the slice stops where the body opens.
        DeclKind::Fn(f) => {
            print_doc(d, &pad);
            let end = f.body.as_ref().map(|b| b.span.start).unwrap_or(d.span.end);
            print_slice(text, d.span.start, end, &pad);
        }
        // An impl says only WHICH protocol it implements for what — one line. The
        // methods' signatures are the protocol's to document; repeating them per impl
        // is noise, and a method has no span of its own to slice by anyway.
        DeclKind::Impl(_) => {
            print_doc(d, &pad);
            let header_end = text[d.span.start..d.span.end]
                .find('{')
                .map(|i| d.span.start + i)
                .unwrap_or(d.span.end);
            print_slice(text, d.span.start, header_end, &pad);
        }
        // Everything else IS its source: a record's fields, a protocol's methods, an
        // alias's right-hand side are the interface, whole.
        _ => {
            print_doc(d, &pad);
            print_slice(text, d.span.start, d.span.end, &pad);
        }
    }
}

fn print_doc(d: &Decl, pad: &str) {
    for line in &d.docs {
        println!("{pad}/// {line}");
    }
}

/// Print a source slice, trimmed of a trailing body-opening brace, re-indented.
fn print_slice(text: &str, start: usize, end: usize, pad: &str) {
    let slice = text[start..end].trim_end().trim_end_matches('{').trim_end();
    for line in slice.lines() {
        println!("{pad}{line}");
    }
    println!();
}
