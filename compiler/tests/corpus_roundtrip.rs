//! The corpus, through the formatter and back.
//!
//! `tests/lang/**/*.neon` is the language specification. Every file in it that
//! parses must survive format -> reparse with the same tree, must format to a
//! fixed point, and must keep every comment. The corpus is where the shapes
//! nobody thought to unit-test live.

use neon_compiler::ast::{strip_spans, Module};
use neon_compiler::{format, lexer, parser};
use std::path::{Path, PathBuf};

fn corpus() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/lang");
    let mut out = Vec::new();
    collect(&root, &mut out);
    out.sort();
    assert!(!out.is_empty(), "no corpus at {}", root.display());
    out
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("corpus directory is readable") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "neon") {
            out.push(path);
        }
    }
}

fn tree(src: &str) -> Option<Module> {
    let lexed = lexer::lex_full(src).ok()?;
    let (module, errors) = parser::parse(&lexed.tokens, src.len());
    if !errors.is_empty() {
        return None;
    }
    let mut m = module?;
    strip_spans(&mut m);
    Some(m)
}

fn comments(src: &str) -> Vec<String> {
    match lexer::lex_full(src) {
        Ok(l) => l
            .trivia
            .iter()
            .map(|t| t.text.trim_end().to_string())
            .filter(|t| !t.is_empty())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Round-trip, idempotence and comment preservation, over every corpus file
/// that parses. The two that do not are `compile-fail` tests, and are expected
/// not to.
#[test]
fn the_corpus_round_trips() {
    let files = corpus();
    let mut parsed = 0;
    let mut round_tripped = 0;
    let mut failures = Vec::new();

    for path in &files {
        let src = std::fs::read_to_string(path).expect("readable");
        let Some(before) = tree(&src) else {
            continue;
        };
        parsed += 1;

        let formatted = match format::format(&src) {
            Ok(f) => f,
            Err(e) => {
                failures.push(format!("{}: did not format: {e:?}", path.display()));
                continue;
            }
        };

        let Some(after) = tree(&formatted) else {
            failures.push(format!("{}: output does not parse", path.display()));
            continue;
        };
        if before != after {
            failures.push(format!("{}: the tree changed", path.display()));
            continue;
        }
        match format::format(&formatted) {
            Ok(twice) if twice == formatted => {}
            Ok(_) => {
                failures.push(format!("{}: not idempotent", path.display()));
                continue;
            }
            Err(e) => {
                failures.push(format!("{}: output did not format: {e:?}", path.display()));
                continue;
            }
        }
        if let Some(lost) = comments(&src).iter().find(|c| !formatted.contains(*c)) {
            failures.push(format!("{}: comment lost: {lost:?}", path.display()));
            continue;
        }
        round_tripped += 1;
    }

    assert!(
        failures.is_empty(),
        "{}/{} of {} corpus files round-tripped\n{}",
        round_tripped,
        parsed,
        files.len(),
        failures.join("\n")
    );
    assert_eq!(round_tripped, parsed);

    // A floor, not a pin. The comment here used to say "it only goes up" over an
    // `==`, which does the opposite: it goes stale on every corpus addition, and it
    // did — `a06da10` added two files and left this red for two commits. `>=` catches
    // the regression this is for (corpus files quietly stopping working) without
    // demanding a bump for the growth it is not for.
    assert!(
        round_tripped >= 189,
        "the corpus shrank: {round_tripped} files round-trip, was at least 189"
    );
    // Only the compile-fail tests that fail at the SYNTAX level are expected not to
    // parse. Most compile-fail tests parse fine and fail later, so this is a small
    // number and a new one is worth noticing — which is why it stays exact.
    assert_eq!(files.len() - parsed, 3, "a different set of files parses now");
}
