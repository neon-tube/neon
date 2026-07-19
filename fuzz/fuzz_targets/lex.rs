//! The lexer never panics, and its spans are honest.
//!
//! Input is interpreted as UTF-8 and rejected if it is not. That is not a
//! weakening: `lex` takes `&str`, so non-UTF-8 is not a reachable state — there
//! is no caller that could produce one. Fuzzing bytes would mean inventing a
//! lossy decode and then reporting crashes on inputs the function cannot be
//! given. The cost is that a fraction of mutations are discarded; the dictionary
//! and the real-program seed corpus keep that fraction small, and it buys a
//! corpus of literal `.neon` text that a human can read and `neon fmt` can be
//! pointed at directly.
//!
//! Multi-byte input still gets exercised — the seeds contain non-ASCII string
//! and rune literals, and the lexer indexes a `&[u8]` while slicing a `&str`,
//! which is exactly the shape that panics on a char boundary.

#![no_main]

use libfuzzer_sys::fuzz_target;
use neon_compiler::lexer;

fuzz_target!(|data: &[u8]| {
    let Ok(src) = std::str::from_utf8(data) else {
        return;
    };

    // `lex` is `lex_full` minus the trivia, so driving `lex_full` covers both.
    let Ok(lexed) = lexer::lex_full(src) else {
        return;
    };

    // Every span slices back to the text that produced it. The doc comment on
    // `Token` promises this and the formatter depends on it: it reprints
    // literals by slicing the source with the token's span. A span that is
    // reversed, out of bounds, or off a char boundary panics here rather than
    // in whichever later pass happens to slice first.
    for spanned in &lexed.tokens {
        let span = &spanned.span;
        assert!(
            span.start <= span.end && span.end <= src.len(),
            "token span {span:?} is not a range within {} bytes: {:?}",
            src.len(),
            spanned.token
        );
        let _ = &src[span.start..span.end];
    }
    for trivia in &lexed.trivia {
        assert!(
            trivia.span.start <= trivia.span.end && trivia.span.end <= src.len(),
            "trivia span {:?} is not a range within {} bytes",
            trivia.span,
            src.len()
        );
        let _ = &src[trivia.span.start..trivia.span.end];
    }
});
