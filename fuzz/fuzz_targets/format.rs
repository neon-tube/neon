//! The formatter's contract: it may change how a program looks and may not
//! change what it means.
//!
//! This is `compiler/tests/corpus_roundtrip.rs` with the corpus replaced by a
//! fuzzer. The corpus pins the contract over shapes someone thought to write;
//! this pins it over shapes nobody did. That distinction is not academic — the
//! formatter has silently changed meaning before, reprinting `1 - (2 - 3)` as
//! `1 - 2 - 3` because it emitted a binary operator's right operand at the
//! parent's precedence instead of its own. A corpus only catches that if
//! someone wrote a right-nested subtraction.
//!
//! Three properties, in the order they are worth finding a counterexample to:
//!
//! 1. **Round-trip.** `parse(format(src))` is `parse(src)`, compared after
//!    `strip_spans` — spans and node ids legitimately differ, nothing else may.
//!    A failure here is a miscompile waiting to happen.
//! 2. **Idempotence.** `format(format(src)) == format(src)`. A failure is
//!    cosmetic but poisons diffs and CI format checks.
//! 3. **Comment preservation.** Every comment in the input appears in the
//!    output. A failure destroys the user's work.
//!
//! Only inputs that lex and parse *cleanly* are in scope. The formatter is not
//! defined on a program with syntax errors, and `format` says so by returning
//! `Err`.

#![no_main]

use libfuzzer_sys::fuzz_target;
use neon_compiler::ast::{strip_spans, Module};
use neon_compiler::{format, lexer, parser};

/// The tree, with everything the formatter is allowed to move erased. `None`
/// when the source does not lex or does not parse cleanly.
fn tree(src: &str) -> Option<Module> {
    let tokens = lexer::lex(src).ok()?;
    let (module, errors) = parser::parse(&tokens, src.len());
    if !errors.is_empty() {
        return None;
    }
    let mut module = module?;
    strip_spans(&mut module);
    Some(module)
}

/// Comment text, trailing whitespace trimmed — the formatter is free to
/// re-indent a comment but not to drop it. Mirrors the corpus test.
fn comments(src: &str) -> Vec<String> {
    match lexer::lex_full(src) {
        Ok(lexed) => lexed
            .trivia
            .iter()
            .map(|t| t.text.trim_end().to_string())
            .filter(|t| !t.is_empty())
            .collect(),
        Err(_) => Vec::new(),
    }
}

fuzz_target!(|data: &[u8]| {
    let Ok(src) = std::str::from_utf8(data) else {
        return;
    };
    let Some(before) = tree(src) else {
        return;
    };

    let formatted = match format::format(src) {
        Ok(formatted) => formatted,
        // The source parsed cleanly, so the formatter had a tree to print. It
        // has no licence to refuse one.
        Err(e) => panic!("input parses but does not format: {e:?}"),
    };

    // 1. Round-trip.
    match tree(&formatted) {
        Some(after) => assert!(
            before == after,
            "formatting changed the tree\n--- before ---\n{src}\n--- after ---\n{formatted}"
        ),
        None => panic!("formatter output does not parse\n--- output ---\n{formatted}"),
    }

    // 2. Idempotence.
    match format::format(&formatted) {
        Ok(twice) => assert!(
            twice == formatted,
            "formatting is not idempotent\n--- once ---\n{formatted}\n--- twice ---\n{twice}"
        ),
        Err(e) => panic!("formatter output does not format: {e:?}"),
    }

    // 3. Comment preservation.
    if let Some(lost) = comments(src).iter().find(|c| !formatted.contains(*c)) {
        panic!("comment lost: {lost:?}\n--- input ---\n{src}\n--- output ---\n{formatted}");
    }
});
