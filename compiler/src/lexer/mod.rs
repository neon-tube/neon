//! Bytes to tokens. Hand-written, single pass, no backtracking.
//!
//! Two things shape the whole design.
//!
//! **String interpolation nests.** `"a #{f("b")} c"` puts a string inside a hole
//! inside a string, and no regular language can describe that. So the lexer
//! carries a `Mode` *stack* rather than a flag, and a string literal comes out
//! as a flat run of tokens (`StrStart StrText InterpStart .. InterpEnd StrEnd`)
//! that the parser reassembles into a tree. Nothing downstream needs a special
//! case for quotes inside holes, or for holes inside holes.
//!
//! **Trivia is not thrown away.** Comments and line starts are collected into a
//! side table, not into the token stream, so the parser's input is exactly the
//! grammar's alphabet and no combinator has to step over a comment. `lex`
//! returns tokens alone for the compiler; `lex_full` returns the side tables
//! too, which is the only reason `neon fmt` can preserve comments and the
//! author's blank lines. See `trivia.rs`.
//!
//! Errors accumulate rather than abort: the lexer runs to the end of the file
//! and returns every complaint at once, because one bad escape should not hide
//! the next twenty tokens' worth of mistakes. Positions are byte offsets into
//! the original `&str` throughout, so a span can always be sliced back out of
//! the source — the formatter depends on that to reprint literals and comments
//! verbatim.

mod error;
mod token;
mod trivia;

#[cfg(test)]
mod tests;

pub use error::{LexError, LexErrorKind};
pub use token::{Span, Spanned, Token};
pub use trivia::{Lexed, Trivia, TriviaKind};

use unicode_normalization::UnicodeNormalization;

/// Where the lexer is. Interpolation nests — `"a #{f("b")} c"` puts a string
/// inside a hole inside a string — so this is a stack, not a flag.
///
/// Each mode remembers where it was opened, so an unterminated construct can
/// point at its opener rather than at EOF.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Code,
    Str { open: usize },
    /// Inside `#{ ... }`, counting braces so a record literal in a hole
    /// (`#{Point { x: 1 }}`) finds its own `}` before the hole's.
    Interp { open: usize, depth: usize },
}

/// Tokens only. The parser wants nothing else.
pub fn lex(source: &str) -> Result<Vec<Spanned>, Vec<LexError>> {
    lex_full(source).map(|l| l.tokens)
}

/// Tokens, comments, and line starts — everything the formatter needs to put a
/// file back the way the author left it.
pub fn lex_full(source: &str) -> Result<Lexed, Vec<LexError>> {
    Lexer::new(source).run()
}

struct Lexer<'a> {
    src: &'a [u8],
    /// Kept for slicing out `&str` when a token carries text.
    text: &'a str,
    pos: usize,
    modes: Vec<Mode>,
    out: Vec<Spanned>,
    trivia: Vec<Trivia>,
    errors: Vec<LexError>,
}

impl<'a> Lexer<'a> {
    fn new(text: &'a str) -> Self {
        Lexer {
            src: text.as_bytes(),
            text,
            pos: 0,
            modes: vec![Mode::Code],
            out: Vec::new(),
            trivia: Vec::new(),
            errors: Vec::new(),
        }
    }

    /// The whole file, in one pass driven by the mode stack.
    ///
    /// Errors are collected, not raised: the loop always reaches EOF, so a
    /// single bad escape does not hide every mistake after it. Only when the
    /// file is clean is the line table built and a `Lexed` returned — nothing
    /// downstream is allowed to see a half-lexed file.
    fn run(mut self) -> Result<Lexed, Vec<LexError>> {
        // A BOM at the very start is consumed silently; anywhere else it is an
        // error naming itself, rather than a mystery character.
        if self.text.starts_with('\u{feff}') {
            self.pos += '\u{feff}'.len_utf8();
        }

        while self.pos < self.src.len() {
            match self.mode() {
                Mode::Code | Mode::Interp { .. } => self.code_token(),
                Mode::Str { .. } => self.string_body(),
            }
        }

        self.report_unclosed();

        if !self.errors.is_empty() {
            return Err(self.errors);
        }
        let mut line_starts = vec![0usize];
        line_starts.extend(
            self.text
                .bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i + 1),
        );
        Ok(Lexed { tokens: self.out, trivia: self.trivia, line_starts })
    }

    /// Report whatever was still open at EOF, blaming the right thing.
    ///
    /// `"value: #{n"` leaves the stack as [Code, Str, Interp, Str]: the closing
    /// quote was consumed as the *opening* quote of a string inside the hole.
    /// The innermost failure is that inner string, but the actual mistake is the
    /// missing `}` — so an unclosed interpolation anywhere outranks a string,
    /// and we point at its `#{` rather than at EOF.
    fn report_unclosed(&mut self) {
        let interp = self.modes.iter().find_map(|m| match m {
            Mode::Interp { open, .. } => Some(*open),
            _ => None,
        });
        if let Some(open) = interp {
            self.err(LexErrorKind::UnterminatedInterp, open..open + 2);
            return;
        }
        let string = self.modes.iter().find_map(|m| match m {
            Mode::Str { open } => Some(*open),
            _ => None,
        });
        if let Some(open) = string {
            self.err(LexErrorKind::UnterminatedString, open..open + 1);
        }
    }

    // ---- position helpers ----

    fn mode(&self) -> Mode {
        *self.modes.last().expect("mode stack is never empty")
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn peek_at(&self, n: usize) -> Option<u8> {
        self.src.get(self.pos + n).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    fn eat(&mut self, b: u8) -> bool {
        if self.peek() == Some(b) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn push(&mut self, token: Token, start: usize) {
        self.out.push(Spanned { token, span: start..self.pos });
    }

    fn err(&mut self, kind: LexErrorKind, span: Span) {
        self.errors.push(LexError::new(kind, span));
    }

    // ---- code mode ----

    fn code_token(&mut self) {
        self.skip_trivia();
        if self.pos >= self.src.len() {
            return;
        }
        let start = self.pos;
        let b = self.peek().expect("checked non-empty");

        match b {
            b'"' => {
                self.pos += 1;
                self.push(Token::StrStart, start);
                self.modes.push(Mode::Str { open: start });
            }
            b'\'' => self.rune(start),
            b'0'..=b'9' => self.number(start),
            b':' if self.atom_ahead() => {
                self.pos += 1;
                let name = self.take_ident_text();
                self.push(Token::Atom(name), start);
            }
            _ if self.at_ident_start() => {
                let word = self.take_ident_text();
                match Token::keyword(&word) {
                    Some(k) => self.push(k, start),
                    None => self.push(Token::Ident(word), start),
                }
            }
            _ if !b.is_ascii() => self.non_ascii(start),
            _ => self.punct(start),
        }
    }

    /// Whitespace, line comments, and nested block comments.
    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\n' | b'\r' | b'\x0c') => {
                    self.pos += 1;
                }
                Some(b'/') if self.peek_at(1) == Some(b'/') => {
                    let start = self.pos;
                    // `///` is a doc comment: it attaches to what follows and a
                    // doc tool will want it. Tagging the kind here is free;
                    // reconstructing it later is not.
                    let doc = self.peek_at(2) == Some(b'/') && self.peek_at(3) != Some(b'/');
                    self.pos += if doc { 3 } else { 2 };
                    let text_start = self.pos;
                    while let Some(c) = self.peek() {
                        if c == b'\n' {
                            break;
                        }
                        self.pos += 1;
                    }
                    self.trivia.push(Trivia {
                        kind: if doc { TriviaKind::Doc } else { TriviaKind::Line },
                        span: start..self.pos,
                        text: self.text[text_start..self.pos].to_string(),
                    });
                }
                Some(b'/') if self.peek_at(1) == Some(b'*') => {
                    let start = self.pos;
                    self.pos += 2;
                    // Nested, so a commented-out block containing a comment does
                    // not end early. A regex cannot count; this is why the
                    // previous lexer needed a custom callback here too.
                    let mut depth = 1usize;
                    while depth > 0 {
                        match self.peek() {
                            None => {
                                self.err(LexErrorKind::UnterminatedBlockComment, start..self.pos);
                                return;
                            }
                            Some(b'/') if self.peek_at(1) == Some(b'*') => {
                                self.pos += 2;
                                depth += 1;
                            }
                            Some(b'*') if self.peek_at(1) == Some(b'/') => {
                                self.pos += 2;
                                depth -= 1;
                            }
                            Some(_) => self.pos += 1,
                        }
                    }
                    self.trivia.push(Trivia {
                        kind: TriviaKind::Block,
                        span: start..self.pos,
                        text: self.text[start + 2..self.pos - 2].to_string(),
                    });
                }
                _ => return,
            }
        }
    }

    /// The char at `pos`, decoded. Only called off the ASCII fast path.
    fn peek_char(&self) -> Option<char> {
        self.text.get(self.pos..)?.chars().next()
    }

    fn at_ident_start(&self) -> bool {
        match self.peek() {
            Some(b) if b.is_ascii() => is_ascii_ident_start(b),
            Some(_) => self.peek_char().is_some_and(unicode_ident::is_xid_start),
            None => false,
        }
    }

    /// A `:` starts an atom only if an identifier follows it immediately AND it
    /// does not directly follow one.
    ///
    /// The look-back is what keeps the language from being whitespace-sensitive.
    /// Without it `{ x:y }` lexes as `x` then the atom `:y` while `{ x: y }`
    /// lexes as `x : y` — the same record literal meaning two different things
    /// depending on a space, and silently. `let x:i64` had the same problem.
    /// A colon glued to an identifier is always punctuation; `m[:key]`, `f(:ok)`
    /// and `x == :ok` all follow something else, so they are unaffected.
    fn atom_ahead(&self) -> bool {
        if self
            .out
            .last()
            .is_some_and(|t| t.span.end == self.pos && matches!(t.token, Token::Ident(_)))
        {
            return false;
        }
        match self.peek_at(1) {
            Some(b) if b.is_ascii() => is_ascii_ident_start(b),
            Some(_) => self.text[self.pos + 1..]
                .chars()
                .next()
                .is_some_and(unicode_ident::is_xid_start),
            None => false,
        }
    }

    /// The identifier at `pos`, normalized to NFC.
    ///
    /// Without normalization `café` (U+00E9) and `café` (`e` + U+0301) are
    /// different identifiers that render identically — a supply-chain hazard,
    /// not a curiosity. NFC is skipped entirely for pure-ASCII words, which is
    /// almost all of them and cannot need it.
    fn take_ident_text(&mut self) -> String {
        let start = self.pos;
        let mut ascii_only = true;
        loop {
            match self.peek() {
                // ASCII is the overwhelming majority; keep it a byte compare.
                Some(b) if b.is_ascii() => {
                    if is_ascii_ident_continue(b) {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                Some(_) => match self.peek_char() {
                    Some(c) if unicode_ident::is_xid_continue(c) => {
                        self.pos += c.len_utf8();
                        ascii_only = false;
                    }
                    _ => break,
                },
                None => break,
            }
        }
        let raw = &self.text[start..self.pos];
        if ascii_only {
            raw.to_string()
        } else {
            raw.nfc().collect()
        }
    }

    /// A non-ASCII character that cannot start an identifier.
    fn non_ascii(&mut self, start: usize) {
        if self.text[start..].starts_with('\u{feff}') {
            self.pos += '\u{feff}'.len_utf8();
            self.err(LexErrorKind::UnexpectedBom, start..self.pos);
            return;
        }
        let c = self.peek_char().expect("non-empty");
        self.pos += c.len_utf8();
        self.err(LexErrorKind::UnexpectedChar(c), start..self.pos);
    }

    fn punct(&mut self, start: usize) {
        let b = self.bump().expect("checked non-empty");
        // Longest match first: `::` before `:`, `..` before `.`, `->` before `-`.
        let tok = match b {
            b'+' => Token::Plus,
            b'-' => {
                if self.eat(b'>') {
                    Token::Arrow
                } else {
                    Token::Minus
                }
            }
            b'*' => Token::Star,
            b'/' => Token::Slash,
            b'%' => Token::Percent,
            b'=' => {
                if self.eat(b'=') {
                    Token::EqEq
                } else if self.eat(b'>') {
                    Token::FatArrow
                } else {
                    Token::Eq
                }
            }
            b'!' => {
                if self.eat(b'=') {
                    Token::NotEq
                } else {
                    Token::Bang
                }
            }
            b'<' => {
                if self.eat(b'=') {
                    Token::LtEq
                } else {
                    Token::Lt
                }
            }
            b'>' => {
                if self.eat(b'=') {
                    Token::GtEq
                } else {
                    Token::Gt
                }
            }
            b'|' => {
                if self.eat(b'>') {
                    Token::Pipe
                } else {
                    Token::Bar
                }
            }
            b'.' => {
                if self.eat(b'.') {
                    Token::DotDot
                } else {
                    Token::Dot
                }
            }
            b':' => {
                if self.eat(b':') {
                    Token::ColonColon
                } else {
                    Token::Colon
                }
            }
            b'?' => Token::Question,
            b',' => Token::Comma,
            b';' => Token::Semi,
            b'@' => Token::At,
            b'&' => Token::Ampersand,
            b'(' => Token::LParen,
            b')' => Token::RParen,
            b'[' => Token::LBracket,
            b']' => Token::RBracket,
            b'{' => {
                if let Mode::Interp { open, depth } = self.mode() {
                    *self.modes.last_mut().expect("mode") = Mode::Interp { open, depth: depth + 1 };
                }
                Token::LBrace
            }
            b'}' => {
                match self.mode() {
                    // Depth 0 inside a hole: this `}` closes the hole itself.
                    Mode::Interp { depth: 0, .. } => {
                        self.modes.pop();
                        self.push(Token::InterpEnd, start);
                        return;
                    }
                    Mode::Interp { open, depth } => {
                        *self.modes.last_mut().expect("mode") = Mode::Interp { open, depth: depth - 1 };
                    }
                    _ => {}
                }
                Token::RBrace
            }
            _ => {
                let c = self.text[start..].chars().next().expect("non-empty");
                self.pos = start + c.len_utf8();
                self.err(LexErrorKind::UnexpectedChar(c), start..self.pos);
                return;
            }
        };
        self.push(tok, start);
    }

    // ---- string mode ----

    /// One run of string content, up to whichever comes first: the closing
    /// quote, a `#{` hole, or EOF.
    ///
    /// It returns at each of those rather than looping, because all three change
    /// the mode stack and the caller re-dispatches on the new mode. Escapes are
    /// decoded into the `StrText` payload here, so the token carries the *value*;
    /// the formatter recovers the author's spelling from the span instead.
    fn string_body(&mut self) {
        let start = self.pos;
        let mut text = String::new();

        loop {
            match self.peek() {
                None => {
                    if !text.is_empty() {
                        self.push(Token::StrText(text), start);
                    }
                    // run() reports the unterminated string; just leave.
                    return;
                }
                Some(b'"') => {
                    if !text.is_empty() {
                        self.push(Token::StrText(text), start);
                    }
                    let q = self.pos;
                    self.pos += 1;
                    self.modes.pop();
                    self.push(Token::StrEnd, q);
                    return;
                }
                // `#{` opens a hole. A bare `#` is literal text, which is the
                // whole point of this delimiter: `{` never needs escaping.
                Some(b'#') if self.peek_at(1) == Some(b'{') => {
                    if !text.is_empty() {
                        self.push(Token::StrText(text), start);
                    }
                    let h = self.pos;
                    self.pos += 2;
                    self.push(Token::InterpStart, h);
                    self.modes.push(Mode::Interp { open: h, depth: 0 });
                    return;
                }
                Some(b'\\') => {
                    let esc = self.pos;
                    self.pos += 1;
                    // `\#` escapes an interpolation; `\#{` is a literal `#{`.
                    if self.eat(b'#') {
                        text.push('#');
                        continue;
                    }
                    // On a bad escape `escape` has already reported; keep going
                    // so one typo does not swallow the rest of the file.
                    if let Some(c) = self.escape(esc) {
                        text.push(c);
                    }
                }
                Some(_) => {
                    let c = self.text[self.pos..].chars().next().expect("non-empty");
                    self.pos += c.len_utf8();
                    text.push(c);
                }
            }
        }
    }

    // ---- literals ----

    /// The escape body, positioned just after the backslash. Shared by strings
    /// and runes — the previous implementation had two near-identical copies,
    /// and they had already drifted.
    fn escape(&mut self, backslash: usize) -> Option<char> {
        let c = match self.bump() {
            None => {
                self.err(LexErrorKind::UnterminatedString, backslash..self.pos);
                return None;
            }
            Some(c) => c,
        };
        Some(match c {
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'0' => '\0',
            b'\\' => '\\',
            b'"' => '"',
            b'\'' => '\'',
            b'x' => {
                let h = self.pos;
                let mut v = 0u32;
                for _ in 0..2 {
                    match self.peek().filter(|c| c.is_ascii_hexdigit()) {
                        Some(d) => {
                            self.pos += 1;
                            v = v * 16 + (d as char).to_digit(16).expect("hex");
                        }
                        None => {
                            self.err(LexErrorKind::BadHexEscape, backslash..self.pos.max(h));
                            return None;
                        }
                    }
                }
                // Always a scalar value: \x is capped at 0xFF.
                char::from_u32(v).expect("0..=0xff is a scalar value")
            }
            b'u' => {
                if !self.eat(b'{') {
                    self.err(LexErrorKind::BadUnicodeEscape, backslash..self.pos);
                    return None;
                }
                let mut v: u32 = 0;
                let mut digits = 0;
                loop {
                    match self.peek() {
                        Some(b'}') => {
                            self.pos += 1;
                            break;
                        }
                        Some(d) if d.is_ascii_hexdigit() && digits < 6 => {
                            self.pos += 1;
                            digits += 1;
                            v = v * 16 + (d as char).to_digit(16).expect("hex");
                        }
                        _ => {
                            self.err(LexErrorKind::BadUnicodeEscape, backslash..self.pos);
                            return None;
                        }
                    }
                }
                if digits == 0 {
                    self.err(LexErrorKind::BadUnicodeEscape, backslash..self.pos);
                    return None;
                }
                match char::from_u32(v) {
                    Some(c) => c,
                    None => {
                        // Surrogates and > 0x10FFFF are not characters.
                        self.err(LexErrorKind::BadUnicodeEscape, backslash..self.pos);
                        return None;
                    }
                }
            }
            other => {
                // `bump` advanced one *byte*, but what is being escaped is a
                // character. For a multi-byte one that leaves `pos` inside a UTF-8
                // sequence, and the caller resumes with `self.text[self.pos..]`,
                // which panics on a char boundary — `"a\éb"` crashed the compiler.
                // Re-read the character whole, which also fixes the diagnostic:
                // `other as char` on a lead byte names some other character
                // entirely (`Ã` for `é`).
                let at = self.pos - 1;
                let ch = self.text[at..].chars().next().unwrap_or(other as char);
                self.pos = at + ch.len_utf8();
                self.err(LexErrorKind::UnknownEscape(ch), backslash..self.pos);
                return None;
            }
        })
    }

    fn rune(&mut self, start: usize) {
        self.pos += 1; // opening quote
        let c = match self.peek() {
            None => {
                self.err(LexErrorKind::UnterminatedRune, start..self.pos);
                return;
            }
            Some(b'\'') => {
                self.pos += 1;
                self.err(LexErrorKind::EmptyRune, start..self.pos);
                return;
            }
            Some(b'\\') => {
                let esc = self.pos;
                self.pos += 1;
                match self.escape(esc) {
                    Some(c) => c,
                    None => {
                        // Consume to the closing quote so one bad rune does not
                        // cascade into every token after it.
                        while let Some(b) = self.peek() {
                            self.pos += 1;
                            if b == b'\'' {
                                break;
                            }
                        }
                        return;
                    }
                }
            }
            Some(_) => {
                let c = self.text[self.pos..].chars().next().expect("non-empty");
                self.pos += c.len_utf8();
                c
            }
        };
        if self.eat(b'\'') {
            self.push(Token::Rune(c), start);
        } else {
            // 'ab' and 'a are different mistakes; say which.
            while let Some(b) = self.peek() {
                if b == b'\'' {
                    self.pos += 1;
                    self.err(LexErrorKind::OvershortRune, start..self.pos);
                    return;
                }
                if b == b'\n' {
                    break;
                }
                self.pos += 1;
            }
            self.err(LexErrorKind::UnterminatedRune, start..self.pos);
        }
    }

    /// An integer or float literal.
    ///
    /// Only the magnitude is lexed; a leading `-` is a separate token that the
    /// parser folds in. That is what keeps `i64::MIN` writable — see
    /// `Token::Int`.
    ///
    /// A float keeps its *text* rather than its value — underscores stripped,
    /// spelling otherwise intact — so the formatter can reprint `1e0` as the
    /// author wrote it instead of as `1`.
    fn number(&mut self, start: usize) {
        let radix = match (self.peek(), self.peek_at(1)) {
            (Some(b'0'), Some(b'x' | b'X')) => 16,
            (Some(b'0'), Some(b'o' | b'O')) => 8,
            (Some(b'0'), Some(b'b' | b'B')) => 2,
            _ => 10,
        };

        if radix != 10 {
            self.pos += 2;
            let digits = self.take_digits(radix);
            if digits.is_empty() {
                self.err(LexErrorKind::EmptyIntLiteral, start..self.pos);
                return;
            }
            match u64::from_str_radix(&digits, radix) {
                Ok(v) => self.push(Token::Int(v), start),
                Err(_) => self.err(LexErrorKind::IntegerOverflow, start..self.pos),
            }
            return;
        }

        let int_part = self.take_digits(10);

        // A float needs a digit after the dot, so `xs..1` and `x.0` stay
        // unambiguous against `..` and field access.
        let is_float = (self.peek() == Some(b'.')
            && matches!(self.peek_at(1), Some(d) if d.is_ascii_digit()))
            || matches!(self.peek(), Some(b'e' | b'E'));

        if !is_float {
            match int_part.parse::<u64>() {
                Ok(v) => self.push(Token::Int(v), start),
                Err(_) => self.err(LexErrorKind::IntegerOverflow, start..self.pos),
            }
            return;
        }

        if self.eat(b'.') {
            let frac = self.take_digits(10);
            if frac.is_empty() {
                self.err(LexErrorKind::EmptyIntLiteral, start..self.pos);
                return;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            let exp = self.take_digits(10);
            if exp.is_empty() {
                self.err(LexErrorKind::EmptyIntLiteral, start..self.pos);
                return;
            }
        }
        let text = self.text[start..self.pos].replace('_', "");
        self.push(Token::Float(text), start);
    }

    /// Digits with `_` separators stripped. An underscore must sit between two
    /// digits: `1_000` is fine, `_1`, `1_` and `1__0` are not.
    fn take_digits(&mut self, radix: u32) -> String {
        let mut s = String::new();
        let mut last_was_digit = false;
        loop {
            match self.peek() {
                Some(b'_') => {
                    // Consume the whole run, so `1__0` is one complaint.
                    let at = self.pos;
                    while self.peek() == Some(b'_') {
                        self.pos += 1;
                    }
                    let next_is_digit =
                        matches!(self.peek(), Some(d) if (d as char).is_digit(radix));
                    let lone = self.pos - at == 1;
                    if !last_was_digit || !next_is_digit || !lone {
                        self.err(LexErrorKind::MisplacedUnderscore, at..self.pos);
                    }
                    last_was_digit = false;
                }
                Some(d) if (d as char).is_digit(radix) => {
                    self.pos += 1;
                    s.push(d as char);
                    last_was_digit = true;
                }
                _ => return s,
            }
        }
    }
}

fn is_ascii_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ascii_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}
