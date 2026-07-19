//! The parser never unwinds, whatever it is handed.
//!
//! `parse` returns `(Option<Module>, Vec<Error>)`: a syntax error is a value,
//! not a panic, and the CLI's no-panic-on-user-input guarantee (#102) rests on
//! that. Anything that gets past the lexer is legal input here, including token
//! streams that are complete nonsense.
//!
//! Note the second argument is `src.len()`, the EOF offset. Passing the byte
//! length of the *source* rather than the token count is what lets the parser
//! point an "unexpected end of input" at the end of the file, and it is a
//! standing chance for an off-by-one to become an out-of-bounds slice.

#![no_main]

use libfuzzer_sys::fuzz_target;
use neon_compiler::ast::strip_spans;
use neon_compiler::{lexer, parser};

fuzz_target!(|data: &[u8]| {
    let Ok(src) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(tokens) = lexer::lex(src) else {
        return;
    };

    let (module, errors) = parser::parse(&tokens, src.len());

    // A parse either produced a module or reported why it could not. Silently
    // returning neither would leave a caller with nothing to print.
    assert!(
        module.is_some() || !errors.is_empty(),
        "parse returned no module and no error"
    );

    // Every error span must be sliceable, for the same reason token spans must
    // be: `diagnostic` renders them against the source.
    for error in &errors {
        let span = &error.span;
        assert!(
            span.start <= span.end && span.end <= src.len(),
            "error span {span:?} is not a range within {} bytes",
            src.len()
        );
        let _ = &src[span.start..span.end];
    }

    // Walking the tree is not free of consequence: `strip_spans` recurses over
    // every node, so a parser that built a pathologically deep or cyclic tree
    // is caught here rather than in typecheck.
    if let Some(mut module) = module {
        strip_spans(&mut module);
    }
});
