use super::*;

fn toks(src: &str) -> Vec<Token> {
    lex(src).expect("lexes").into_iter().map(|s| s.token).collect()
}

fn errs(src: &str) -> Vec<LexErrorKind> {
    lex(src).expect_err("should fail").into_iter().map(|e| e.kind).collect()
}

#[test]
fn keywords_and_identifiers() {
    assert_eq!(
        toks("fn let mu type newtype opaque"),
        vec![Token::Fn, Token::Let, Token::Mu, Token::Type, Token::Newtype, Token::Opaque]
    );
    // A word that merely starts with a keyword is an identifier.
    assert_eq!(toks("iffy lettuce ismail"), vec![
        Token::Ident("iffy".into()),
        Token::Ident("lettuce".into()),
        Token::Ident("ismail".into()),
    ]);
}

#[test]
fn enum_is_not_a_keyword() {
    // Sum types are unions of records; `enum` is an ordinary identifier. The
    // parser gives a dedicated diagnostic when it appears as a declaration.
    assert_eq!(toks("enum"), vec![Token::Ident("enum".into())]);
}

#[test]
fn atoms() {
    assert_eq!(toks(":ok :err_2 :A"), vec![
        Token::Atom("ok".into()),
        Token::Atom("err_2".into()),
        Token::Atom("A".into()),
    ]);
    // `::` is a path separator, not an atom; and `:` before a non-identifier is
    // just a colon.
    assert_eq!(toks("a::b"), vec![
        Token::Ident("a".into()),
        Token::ColonColon,
        Token::Ident("b".into()),
    ]);
    assert_eq!(toks("{ x: 1 }"), vec![
        Token::LBrace,
        Token::Ident("x".into()),
        Token::Colon,
        Token::Int(1),
        Token::RBrace,
    ]);
}

#[test]
fn i64_min_is_lexable() {
    // The whole reason Int carries a u64 magnitude. As an i64 token this
    // overflows before the parser can fold the unary minus, which is what made
    // i64::MIN unwritable in the previous implementation.
    assert_eq!(
        toks("-9223372036854775808"),
        vec![Token::Minus, Token::Int(9223372036854775808)]
    );
    assert_eq!(errs("18446744073709551616"), vec![LexErrorKind::IntegerOverflow]);
}

#[test]
fn integer_bases_and_separators() {
    assert_eq!(toks("0xFF 0o17 0b1010 1_000_000"), vec![
        Token::Int(255),
        Token::Int(15),
        Token::Int(10),
        Token::Int(1_000_000),
    ]);
    assert_eq!(errs("0x"), vec![LexErrorKind::EmptyIntLiteral]);
    assert_eq!(errs("1__0"), vec![LexErrorKind::MisplacedUnderscore]);
    assert_eq!(errs("1_"), vec![LexErrorKind::MisplacedUnderscore]);
}

#[test]
fn floats_versus_dots() {
    assert_eq!(toks("1.5 2e10 1.5e-3"), vec![
        Token::Float("1.5".into()),
        Token::Float("2e10".into()),
        Token::Float("1.5e-3".into()),
    ]);
    // A float needs a digit after the dot, so spread and field access stay
    // unambiguous.
    assert_eq!(toks("{ ..a }"), vec![
        Token::LBrace,
        Token::DotDot,
        Token::Ident("a".into()),
        Token::RBrace,
    ]);
    assert_eq!(toks("t.0"), vec![Token::Ident("t".into()), Token::Dot, Token::Int(0)]);
}

#[test]
fn plain_string() {
    assert_eq!(toks(r#""hi""#), vec![
        Token::StrStart,
        Token::StrText("hi".into()),
        Token::StrEnd,
    ]);
    assert_eq!(toks(r#""""#), vec![Token::StrStart, Token::StrEnd]);
}

#[test]
fn braces_in_strings_are_literal() {
    // The point of `#{`: `{` never needs escaping, so JSON and CSS in a string
    // just work.
    assert_eq!(toks(r#""{ \"json\": true }""#), vec![
        Token::StrStart,
        Token::StrText("{ \"json\": true }".into()),
        Token::StrEnd,
    ]);
}

#[test]
fn interpolation() {
    assert_eq!(toks(r#""a #{x} b""#), vec![
        Token::StrStart,
        Token::StrText("a ".into()),
        Token::InterpStart,
        Token::Ident("x".into()),
        Token::InterpEnd,
        Token::StrText(" b".into()),
        Token::StrEnd,
    ]);
}

#[test]
fn interpolation_holds_a_record_literal() {
    // Brace matching: the hole's `}` is found only after the record's. This is
    // the case that made `{}` delimiters ambiguous and forced `#{}`.
    assert_eq!(toks(r##""#{Point { x: 1 }}""##), vec![
        Token::StrStart,
        Token::InterpStart,
        Token::Ident("Point".into()),
        Token::LBrace,
        Token::Ident("x".into()),
        Token::Colon,
        Token::Int(1),
        Token::RBrace,
        Token::InterpEnd,
        Token::StrEnd,
    ]);
}

#[test]
fn interpolation_holds_a_string() {
    // The mode stack earning its keep: a string inside a hole inside a string.
    assert_eq!(toks(r#""a #{f("b")} c""#), vec![
        Token::StrStart,
        Token::StrText("a ".into()),
        Token::InterpStart,
        Token::Ident("f".into()),
        Token::LParen,
        Token::StrStart,
        Token::StrText("b".into()),
        Token::StrEnd,
        Token::RParen,
        Token::InterpEnd,
        Token::StrText(" c".into()),
        Token::StrEnd,
    ]);
}

#[test]
fn escaped_interpolation_is_literal() {
    assert_eq!(toks(r#""\#{x}""#), vec![
        Token::StrStart,
        Token::StrText("#{x}".into()),
        Token::StrEnd,
    ]);
    // A bare `#` not followed by `{` is just text.
    assert_eq!(toks(r##""a # b""##), vec![
        Token::StrStart,
        Token::StrText("a # b".into()),
        Token::StrEnd,
    ]);
}

#[test]
fn unterminated_interpolation_and_string() {
    assert_eq!(errs(r#""a #{x"#), vec![LexErrorKind::UnterminatedInterp]);
    assert_eq!(errs(r#""abc"#), vec![LexErrorKind::UnterminatedString]);
}

#[test]
fn a_missing_close_brace_blames_the_interpolation() {
    // `"value: #{n"` — the closing quote is consumed as the OPENING quote of a
    // string inside the hole, so the stack ends [Code, Str, Interp, Str] and the
    // innermost failure is that inner string. The actual mistake is the missing
    // `}`, so an unclosed interpolation outranks a string.
    let e = lex(r#"("value: #{n")"#).expect_err("fails");
    assert_eq!(e.len(), 1);
    assert_eq!(e[0].kind, LexErrorKind::UnterminatedInterp);
    // And it points at the `#{`, not at EOF.
    assert_eq!(e[0].span, 9..11);
}

#[test]
fn unterminated_string_points_at_its_opening_quote() {
    let e = lex(r#"let s = "abc"#).expect_err("fails");
    assert_eq!(e[0].kind, LexErrorKind::UnterminatedString);
    assert_eq!(e[0].span, 8..9);
}

#[test]
fn string_escapes() {
    assert_eq!(toks(r#""\n\t\\\"\x41\u{1F600}""#), vec![
        Token::StrStart,
        Token::StrText("\n\t\\\"A\u{1F600}".into()),
        Token::StrEnd,
    ]);
}

#[test]
fn unknown_escape_is_an_error() {
    // The previous implementation kept `\q` as a literal backslash-q, so a typo
    // compiled and shipped.
    assert_eq!(errs(r#""\q""#), vec![LexErrorKind::UnknownEscape('q')]);
}

#[test]
fn bad_escapes() {
    assert_eq!(errs(r#""\x4""#), vec![LexErrorKind::BadHexEscape]);
    assert_eq!(errs(r#""\xZZ""#), vec![LexErrorKind::BadHexEscape]);
    // Missing closing brace: this silently lexed as "A" before.
    assert_eq!(errs(r#""\u{41""#), vec![LexErrorKind::BadUnicodeEscape]);
    assert_eq!(errs(r#""\u{}""#), vec![LexErrorKind::BadUnicodeEscape]);
    // A surrogate is not a character.
    assert_eq!(errs(r#""\u{D800}""#), vec![LexErrorKind::BadUnicodeEscape]);
}

#[test]
fn runes() {
    assert_eq!(toks(r"'a' '\n' '\x41' '\u{1F600}'"), vec![
        Token::Rune('a'),
        Token::Rune('\n'),
        Token::Rune('A'),
        Token::Rune('\u{1F600}'),
    ]);
    assert_eq!(toks("'é'"), vec![Token::Rune('é')]);
}

#[test]
fn bad_runes() {
    assert_eq!(errs("''"), vec![LexErrorKind::EmptyRune]);
    assert_eq!(errs("'ab'"), vec![LexErrorKind::OvershortRune]);
    assert_eq!(errs("'a"), vec![LexErrorKind::UnterminatedRune]);
}

#[test]
fn nested_block_comments() {
    // A regex cannot count, which is why this is hand-written.
    assert_eq!(toks("/* a /* b */ c */ 1"), vec![Token::Int(1)]);
    assert_eq!(errs("/* /* */"), vec![LexErrorKind::UnterminatedBlockComment]);
}

#[test]
fn line_comments_and_whitespace() {
    assert_eq!(toks("1 // two\n3"), vec![Token::Int(1), Token::Int(3)]);
}

#[test]
fn longest_match_punctuation() {
    assert_eq!(toks("-> - == = => != ! :: : .. . |> |"), vec![
        Token::Arrow,
        Token::Minus,
        Token::EqEq,
        Token::Eq,
        Token::FatArrow,
        Token::NotEq,
        Token::Bang,
        Token::ColonColon,
        Token::Colon,
        Token::DotDot,
        Token::Dot,
        Token::Pipe,
        Token::Bar,
    ]);
}

#[test]
fn try_forms_lex_as_pieces() {
    // `try?` and `try!` are the parser's job to assemble; the lexer must not
    // read `try! f()` as `try (!f())`, which is how the old parser accidentally
    // accepted it.
    assert_eq!(toks("try? try! try"), vec![
        Token::Try,
        Token::Question,
        Token::Try,
        Token::Bang,
        Token::Try,
    ]);
}

#[test]
fn unicode_identifiers() {
    // UAX #31 XID_Start / XID_Continue, same rule Rust uses. Source is UTF-8
    // and so are identifiers.
    assert_eq!(toks("let café = 1"), vec![
        Token::Let,
        Token::Ident("café".into()),
        Token::Eq,
        Token::Int(1),
    ]);
    assert_eq!(toks("日本語"), vec![Token::Ident("日本語".into())]);
    assert_eq!(toks("_Ω2"), vec![Token::Ident("_Ω2".into())]);
    // Atoms follow identifier rules.
    assert_eq!(toks(":café"), vec![Token::Atom("café".into())]);
}

#[test]
fn unicode_that_cannot_start_an_identifier() {
    // An emoji is XID_Continue-less and XID_Start-less: not an identifier, and
    // not punctuation either.
    assert_eq!(errs("let 🦀 = 1"), vec![LexErrorKind::UnexpectedChar('🦀')]);
}

#[test]
fn identifiers_are_nfc_normalized() {
    // Composed U+00E9 and decomposed `e` + U+0301 render identically. Without
    // normalization they are different identifiers, which is a way to smuggle a
    // second definition past a reader.
    let composed = "caf\u{e9}";
    let decomposed = "cafe\u{301}";
    assert_ne!(composed, decomposed, "the test inputs must differ as bytes");
    assert_eq!(toks(composed), toks(decomposed));
    assert_eq!(toks(decomposed), vec![Token::Ident(composed.into())]);
    // Atoms too — they share the identifier rules.
    assert_eq!(toks(":cafe\u{301}"), vec![Token::Atom(composed.into())]);
}

#[test]
fn nfc_does_not_disturb_ascii() {
    // The fast path: ASCII words skip normalization entirely.
    assert_eq!(toks("hello_world2"), vec![Token::Ident("hello_world2".into())]);
}

#[test]
fn bom_only_at_the_start() {
    assert_eq!(toks("\u{feff}fn"), vec![Token::Fn]);
    assert_eq!(errs("fn \u{feff}"), vec![LexErrorKind::UnexpectedBom]);
}

#[test]
fn spans_point_at_the_token() {
    let out = lex("let x = 42").expect("lexes");
    assert_eq!(out[0].span, 0..3);
    assert_eq!(out[1].span, 4..5);
    assert_eq!(out[3].span, 8..10);
}

#[test]
fn errors_accumulate() {
    // One bad token must not stop the lexer: a diagnostics pass wants them all.
    let e = errs(r#""\q" "\w""#);
    assert_eq!(e, vec![LexErrorKind::UnknownEscape('q'), LexErrorKind::UnknownEscape('w')]);
}

#[test]
fn test_and_bench_are_keywords() {
    assert_eq!(toks(r#"test "adds" { assert_eq(1, 1) }"#), vec![
        Token::Test,
        Token::StrStart,
        Token::StrText("adds".into()),
        Token::StrEnd,
        Token::LBrace,
        Token::AssertEq,
        Token::LParen,
        Token::Int(1),
        Token::Comma,
        Token::Int(1),
        Token::RParen,
        Token::RBrace,
    ]);
    assert_eq!(toks("bench"), vec![Token::Bench]);
}

#[test]
fn comments_are_retained_as_trivia() {
    // Dropped comments mean `neon fmt` can only ever delete them. The parser
    // still sees tokens only; trivia rides alongside.
    let l = lex_full("fn f() {} // trailing\n/// doc\n/* block */ fn g() {}")
        .expect("lexes");
    assert_eq!(l.trivia.len(), 3);
    assert_eq!(l.trivia[0].kind, TriviaKind::Line);
    assert_eq!(l.trivia[0].text, " trailing");
    // `///` attaches to what follows and a doc tool will want it, so the kind is
    // tagged at lex time rather than reconstructed later.
    assert_eq!(l.trivia[1].kind, TriviaKind::Doc);
    assert_eq!(l.trivia[1].text, " doc");
    assert_eq!(l.trivia[2].kind, TriviaKind::Block);
    assert_eq!(l.trivia[2].text, " block ");
    // The token stream is unchanged.
    assert!(l.tokens.iter().all(|t| !matches!(t.token, Token::StrText(_))));
}

#[test]
fn four_slashes_is_not_a_doc_comment() {
    let l = lex_full("//// separator").expect("lexes");
    assert_eq!(l.trivia[0].kind, TriviaKind::Line);
}

#[test]
fn blank_lines_between_items_are_recoverable() {
    // A formatter that reflows every blank line on first run is not one people
    // will use. Line starts make this derivable without recording whitespace.
    let src = "fn a() {}\n\n\nfn b() {}";
    let l = lex_full(src).expect("lexes");
    let a_end = l.tokens.iter().find(|t| t.token == Token::RBrace).expect("a's brace").span.end;
    let b_start = l.tokens.iter().rev().find(|t| t.token == Token::Fn).expect("fn b").span.start;
    assert_eq!(l.blank_lines_between(a_end, b_start), 2);

    let l = lex_full("fn a() {}\nfn b() {}").expect("lexes");
    let a_end = l.tokens.iter().find(|t| t.token == Token::RBrace).expect("a's brace").span.end;
    let b_start = l.tokens.iter().rev().find(|t| t.token == Token::Fn).expect("fn b").span.start;
    assert_eq!(l.blank_lines_between(a_end, b_start), 0);
}

#[test]
fn nested_block_comment_text_survives() {
    let l = lex_full("/* a /* b */ c */").expect("lexes");
    assert_eq!(l.trivia.len(), 1);
    assert_eq!(l.trivia[0].text, " a /* b */ c ");
}

#[test]
fn a_colon_glued_to_an_identifier_is_punctuation() {
    // Without the look-back the lexer is whitespace-sensitive: `{ x:y }` lexed
    // as `x` then the atom `:y`, while `{ x: y }` lexed as `x : y` — the same
    // record literal meaning two different things depending on a space, and
    // silently. `let x:i64` had the same problem.
    assert_eq!(toks("{ x:y }"), vec![
        Token::LBrace,
        Token::Ident("x".into()),
        Token::Colon,
        Token::Ident("y".into()),
        Token::RBrace,
    ]);
    assert_eq!(toks("{ x:y }"), toks("{ x: y }"));
    assert_eq!(toks("let x:i64"), toks("let x: i64"));
}

#[test]
fn an_atom_after_anything_else_still_lexes() {
    // The look-back only fires on an identifier, so every real atom position is
    // unaffected.
    assert_eq!(toks("m[:key]"), vec![
        Token::Ident("m".into()),
        Token::LBracket,
        Token::Atom("key".into()),
        Token::RBracket,
    ]);
    assert_eq!(toks("f(:ok)"), vec![
        Token::Ident("f".into()),
        Token::LParen,
        Token::Atom("ok".into()),
        Token::RParen,
    ]);
    assert_eq!(toks("x ==:ok"), vec![
        Token::Ident("x".into()),
        Token::EqEq,
        Token::Atom("ok".into()),
    ]);
    assert_eq!(toks("[:a, :b]"), vec![
        Token::LBracket,
        Token::Atom("a".into()),
        Token::Comma,
        Token::Atom("b".into()),
        Token::RBracket,
    ]);
}
