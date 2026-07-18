use super::*;
use crate::ast::strip_spans;

fn fmt(src: &str) -> String {
    match format(src) {
        Ok(s) => s,
        Err(e) => panic!("{src}\n-- did not format: {e:?}"),
    }
}

fn tree(src: &str) -> Module {
    let lexed = lexer::lex_full(src).expect("lexes");
    let (module, errors) = parser::parse(&lexed.tokens, src.len());
    assert!(errors.is_empty(), "{src}\n-- {errors:?}");
    let mut m = module.expect("parses");
    strip_spans(&mut m);
    m
}

/// The three properties, on one input.
#[track_caller]
fn check(src: &str) -> String {
    let once = fmt(src);
    assert_eq!(tree(src), tree(&once), "round-trip changed the tree:\n{once}");
    assert_eq!(once, fmt(&once), "not idempotent:\n{once}");
    for c in comments(src) {
        assert!(once.contains(&c), "comment lost: {c:?}\n{once}");
    }
    once
}

fn comments(src: &str) -> Vec<String> {
    lexer::lex_full(src)
        .expect("lexes")
        .trivia
        .iter()
        .map(|t| t.text.trim_end().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Formatting the given source produces exactly `want`, and all three
/// properties hold on the way.
#[track_caller]
fn pin(src: &str, want: &str) {
    assert_eq!(check(src), want);
}

// ---- the ladder ----
//
// The bug this whole design exists to prevent: a formatter with its own
// precedence table drops the parentheses that were carrying the meaning.

#[test]
fn parentheses_that_carry_meaning_survive() {
    pin("fn f() { let x = 1 - (2 - 3); }\n", "fn f() { let x = 1 - (2 - 3); }\n");
    pin("fn f() { let x = (1 - 2) - 3; }\n", "fn f() { let x = 1 - 2 - 3; }\n");
    pin("fn f() { let x = (a + b) * c; }\n", "fn f() { let x = (a + b) * c; }\n");
    pin("fn f() { let x = a + b * c; }\n", "fn f() { let x = a + b * c; }\n");
    pin("fn f() { let x = (a or b) and c; }\n", "fn f() { let x = (a or b) and c; }\n");
    pin("fn f() { let x = a or b and c; }\n", "fn f() { let x = a or b and c; }\n");
    pin("fn f() { let x = (1 bsl 2) bsl 3; }\n", "fn f() { let x = 1 bsl 2 bsl 3; }\n");
    pin("fn f() { let x = 1 bsl (2 bsl 3); }\n", "fn f() { let x = 1 bsl (2 bsl 3); }\n");
    // `|>` binds tighter than comparison, so this needs no parentheses.
    pin("fn f() { let x = (x |> g()) == 3; }\n", "fn f() { let x = x |> g() == 3; }\n");
    pin("fn f() { let x = x |> (g() == 3); }\n", "fn f() { let x = x |> (g() == 3); }\n");
    pin("fn f() { let x = a orelse b orelse c; }\n", "fn f() { let x = a orelse b orelse c; }\n");
    pin("fn f() { let x = a orelse (b orelse c); }\n", "fn f() { let x = a orelse (b orelse c); }\n");
}

/// Every pair of levels, both nestings. Reparsing the output has to give the
/// tree back, which is what `check` asserts.
#[test]
fn every_level_against_every_other() {
    let ops = ["orelse", "or", "and", "==", "|>", "bor", "bxor", "band", "bsl", "+", "*"];
    for a in ops {
        for b in ops {
            check(&format!("fn f() {{ let x = p {a} (q {b} r); }}\n"));
            check(&format!("fn f() {{ let x = (p {a} q) {b} r; }}\n"));
            check(&format!("fn f() {{ let x = p {a} q {b} r; }}\n"));
        }
    }
}

#[test]
fn postfix_binds_tighter_than_unary() {
    // `-x.f` is `-(x.f)`, as in C and Rust, so neither reading needs parens.
    pin("fn f() { let a = -x.f; }\n", "fn f() { let a = -x.f; }\n");
    pin("fn f() { let a = -(x.f); }\n", "fn f() { let a = -x.f; }\n");
    pin("fn f() { let a = !g(x); }\n", "fn f() { let a = !g(x); }\n");
    pin("fn f() { let a = !(g(x)); }\n", "fn f() { let a = !g(x); }\n");
    pin("fn f() { let a = bnot x[0]; }\n", "fn f() { let a = bnot x[0]; }\n");
    // The other reading is the one that now needs parentheses.
    pin("fn f() { let a = (-x).f; }\n", "fn f() { let a = (-x).f; }\n");
    pin("fn f() { let a = --x; }\n", "fn f() { let a = -(-x); }\n");
    pin("fn f() { let a = -(-5); }\n", "fn f() { let a = -(-5); }\n");
}

/// `return e` and friends read to the end of the expression, so they need
/// parentheses anywhere but at the loosest position.
#[test]
fn greedy_forms_get_parenthesised() {
    pin("fn f() { let a = (return 1) + 2; }\n", "fn f() { let a = (return 1) + 2; }\n");
    pin("fn f() { let a = (break) - 1; }\n", "fn f() { let a = (break) - 1; }\n");
    pin("fn f() { let a = ((x) => x) (1); }\n", "fn f() { let a = ((x) => x)(1); }\n");
}

/// `try` is NOT one of them: its body is the unary-level parser, so it binds
/// tighter than every binary operator and reads no further than its operand.
#[test]
fn try_is_not_greedy() {
    // The documented easy path, which must need no parentheses to mean what it
    // says: `(try? get(m, k)) orelse 30`.
    pin("fn f() { let a = try g() orelse 3; }\n", "fn f() { let a = try g() orelse 3; }\n");
    pin("fn f() { let a = (try g()) orelse 3; }\n", "fn f() { let a = try g() orelse 3; }\n");
    pin(
        "fn f() { let a = try? get(m, k) orelse 30; }\n",
        "fn f() { let a = try? get(m, k) orelse 30; }\n",
    );
}

#[test]
fn a_record_literal_in_condition_position_keeps_its_parentheses() {
    pin(
        "fn f() { while (P { x: 1 }) { g(); } }\n",
        "fn f() { while (P { x: 1 }) { g(); } }\n",
    );
    pin(
        "fn f() { if (P { x: 1 }).x { g(); } }\n",
        "fn f() { if (P { x: 1 }).x { g(); } }\n",
    );
    // Inside brackets the restriction lifts: no parentheses needed.
    pin(
        "fn f() { if g(P { x: 1 }) { h(); } }\n",
        "fn f() { if g(P { x: 1 }) { h(); } }\n",
    );
}

// ---- comments ----

#[test]
fn comments_of_every_kind_survive() {
    pin(
        "\
// leading
/// a doc comment
fn f() {
    // inside
    let x = 1; // trailing
    /* block */
    g();
    // last thing in the block
}
// end of file
",
        "\
// leading
/// a doc comment
fn f() {
    // inside
    let x = 1;  // trailing
    /* block */
    g();
    // last thing in the block
}
// end of file
",
    );
}

#[test]
fn a_comment_forces_its_construct_open() {
    pin(
        "\
record P {
    // the x
    x: i64,
}
",
        "\
record P {
    // the x
    x: i64,
}
",
    );
}

#[test]
fn a_comment_in_an_empty_block_is_not_dropped() {
    pin("fn f() {\n    // nothing yet\n}\n", "fn f() {\n    // nothing yet\n}\n");
}

#[test]
fn comments_between_and_inside_methods_survive() {
    pin(
        "\
protocol Show for T {
    // required
    fn show(self: T) -> str
}
",
        "\
protocol Show for T {
    // required
    fn show(self: T) -> str
}
",
    );
}

#[test]
fn a_nested_block_comment_keeps_its_nesting() {
    check("/* outer /* inner */ still outer */\nfn f() {}\n");
}

// ---- blank lines ----

#[test]
fn blank_lines_are_kept_and_runs_collapse() {
    pin(
        "use std::io;\nuse std::string;\n\n\n\nfn a() {}\n\n\nfn b() {}\n",
        "use std::io;\nuse std::string;\n\nfn a() {}\n\nfn b() {}\n",
    );
}

#[test]
fn blank_lines_inside_a_block_survive() {
    pin(
        "fn f() {\n    let a = 1;\n\n    let b = 2;\n}\n",
        "fn f() {\n    let a = 1;\n\n    let b = 2;\n}\n",
    );
}

#[test]
fn a_blank_line_after_the_opening_brace_goes_away() {
    pin("fn f() {\n\n    g();\n}\n", "fn f() {\n    g();\n}\n");
}

// ---- literals ----

#[test]
fn strings_are_not_reformatted_inside() {
    // A bare `{` is literal and must not grow an escape; only `#{` is special.
    pin(
        r#"fn f() { let s = "json: { \"literal\": true }"; }
"#,
        r#"fn f() { let s = "json: { \"literal\": true }"; }
"#,
    );
    pin(
        r#"fn f() { let s = "count: #{ n }, #{fmt::pad(x,8)}"; }
"#,
        r#"fn f() { let s = "count: #{n}, #{fmt::pad(x, 8)}"; }
"#,
    );
    // A record literal in a hole is brace-matched, and formats normally.
    pin(
        r##"fn f() { let s = "#{P{x:1,y:2}}"; }
"##,
        r##"fn f() { let s = "#{P { x: 1, y: 2 }}"; }
"##,
    );
    // `\#{` is a literal `#{` and has to stay escaped.
    pin(
        r#"fn f() { let s = "\#{not a hole}"; }
"#,
        r#"fn f() { let s = "\#{not a hole}"; }
"#,
    );
}

/// The AST holds an integer's value and a string's characters, not the text
/// that spelled them. Everything below is recovered from the source; get it
/// wrong and `neon fmt` rewrites `\u{41}` to `A`, which is a different program
/// to read even though it is the same program to run.
#[test]
fn literals_keep_the_spelling_the_author_chose() {
    pin(
        "fn f() { let a = 0xff; let b = 1_000_000; let c = 0b1010; let d = 1.50; let e = 1e0; }\n",
        "fn f() { let a = 0xff; let b = 1_000_000; let c = 0b1010; let d = 1.50; let e = 1e0; }\n",
    );
    pin("fn f() { let a = 1_000.5; }\n", "fn f() { let a = 1_000.5; }\n");
    pin(
        r#"fn f() { let a = "\u{41}\x42"; let b = '\u{43}'; let c = "\t"; }
"#,
        r#"fn f() { let a = "\u{41}\x42"; let b = '\u{43}'; let c = "\t"; }
"#,
    );
    pin(
        "@doc(\"a\\u{41}\") fn f() {}\ntest \"a\\tb\" {}\n",
        "@doc(\"a\\u{41}\") fn f() {}\ntest \"a\\tb\" {}\n",
    );
}

/// The grammar reads `loop { break 7; } + 1` as `(loop { .. }) + 1`. It parses
/// either way; it is printed with the parentheses because nobody should have to
/// know that.
#[test]
fn block_like_operands_keep_their_parentheses() {
    pin(
        "fn f() { show((loop { break 7; }) + 1); }\n",
        "fn f() { show((loop { break 7; }) + 1); }\n",
    );
    pin(
        "fn f() { show(loop { break 7; } + 1); }\n",
        "fn f() { show((loop { break 7; }) + 1); }\n",
    );
    pin(
        "fn f() { let a = \"x\" + (if c { \"y\" } else { \"z\" }); }\n",
        "fn f() { let a = \"x\" + (if c { \"y\" } else { \"z\" }); }\n",
    );
    // Not an operand: no parentheses.
    pin(
        "fn f() { let a = if c { 1 } else { 2 }; }\n",
        "fn f() { let a = if c { 1 } else { 2 }; }\n",
    );
    pin("fn f() { g(if c { 1 } else { 2 }); }\n", "fn f() { g(if c { 1 } else { 2 }); }\n");
}

#[test]
fn i64_min_survives() {
    pin(
        "fn f() { let a = -9223372036854775808; }\n",
        "fn f() { let a = -9223372036854775808; }\n",
    );
}

// ---- declarations ----

#[test]
fn a_function_signature_puts_throws_before_the_arrow() {
    pin(
        "fn get[T](xs: List[T],i: i64)throws IndexError->T where T: Show{ return x; }\n",
        "fn get[T](xs: List[T], i: i64) throws IndexError -> T where T: Show { return x; }\n",
    );
}

#[test]
fn an_arrow_type_puts_throws_before_the_arrow() {
    pin(
        "type H=(i64)throws :error->i64\ntype P = (i64) -> i64\n",
        "type H = (i64) throws :error -> i64\ntype P = (i64) -> i64\n",
    );
}

#[test]
fn an_arrow_type_may_throw_a_union() {
    pin(
        "type H = (i64) throws :a|:b -> i64\n",
        "type H = (i64) throws :a | :b -> i64\n",
    );
}

#[test]
fn type_aliases_take_no_semicolon() {
    pin(
        "type Color=Red|Green\nmu type A=:ok|List[A]\nnewtype Meters=i64\n",
        "type Color = Red | Green\nmu type A = :ok | List[A]\nnewtype Meters = i64\n",
    );
}

#[test]
fn type_operators_keep_their_grouping() {
    pin("type A = (X | Y) & Z\n", "type A = (X | Y) & Z\n");
    pin("type B = X | Y & Z\n", "type B = X | Y & Z\n");
    pin("type C = !(X | Y)\n", "type C = !(X | Y)\n");
    pin("type D = !X & Y\n", "type D = !X & Y\n");
    // A function type reads its return to the end, so a union of one needs
    // brackets.
    pin("type E = ((i64) -> str) | null\n", "type E = ((i64) -> str) | null\n");
    pin("type F = (i64) -> str | null\n", "type F = (i64) -> str | null\n");
}

#[test]
fn declarations_of_every_shape() {
    // An annotation stays on the declaration's line, or above it, as written.
    pin(
        "@native(\"neon_len\")fn len(s: str)->i64{ 0 }\n",
        "@native(\"neon_len\") fn len(s: str) -> i64 { 0 }\n",
    );
    pin(
        "@cfg(\"not(windows)\")\n@doc(\"Spawns.\")\nfn spawn(){}\n",
        "@cfg(\"not(windows)\")\n@doc(\"Spawns.\")\nfn spawn() {}\n",
    );
    // Annotations survive on a protocol, impl and mod, not just fn and record.
    pin(
        "@doc(\"a\") protocol P for T { fn a(v: T) -> i64 }\n",
        "@doc(\"a\") protocol P for T { fn a(v: T) -> i64 }\n",
    );
    pin("@doc(\"m\") mod inner {}\n", "@doc(\"m\") mod inner {\n}\n");
    pin(
        "opaque record Rng[T]{seed: i64}\n",
        "opaque record Rng[T] { seed: i64 }\n",
    );
    pin("record Red{}\n", "record Red {}\n");
    // A body on one line stays on one line, like any other block.
    pin(
        "impl[T]std::fmt::Show for List[T]{fn show(self: List[T])->str{ return \"\"; }}\n",
        "impl[T] std::fmt::Show for List[T] { fn show(self: List[T]) -> str { return \"\"; } }\n",
    );
    pin(
        "impl Show for X {\n    fn show(v: X) -> str { \"X\" }\n}\n",
        "impl Show for X {\n    fn show(v: X) -> str { \"X\" }\n}\n",
    );
    pin(
        "protocol Map for C[_,_]{fn get(self: C)->i64}\n",
        "protocol Map for C[_, _] { fn get(self: C) -> i64 }\n",
    );
    pin(
        "internal mod m{const X: i64=1;}\n",
        "internal mod m {\n    const X: i64 = 1;\n}\n",
    );
    pin(
        "test \"adds two\"{assert_eq(add(1,1),2);}\n",
        "test \"adds two\" { assert_eq(add(1, 1), 2); }\n",
    );
}

// ---- statements and expressions ----

#[test]
fn the_author_decides_where_the_lines_break() {
    // One line in, one line out.
    pin("fn f() { g(1, 2); }\n", "fn f() { g(1, 2); }\n");
    // Broken in, broken out — and the trailing comma is added.
    pin(
        "fn f() {\n    g(\n        1,\n        2\n    );\n}\n",
        "fn f() {\n    g(\n        1,\n        2,\n    );\n}\n",
    );
}

/// A group is broken by a line break at one of its *separators*, not by one of
/// its items happening to span lines.
#[test]
fn a_group_breaks_where_the_author_broke_it() {
    // One arm, on one line, but the braces are the author's: it stays open.
    pin(
        "fn f() {\n    match s {\n        is Circle => \"circle\",\n    }\n}\n",
        "fn f() {\n    match s {\n        is Circle => \"circle\",\n    }\n}\n",
    );
    // A single multi-line argument does not break the argument list around it.
    pin(
        "fn f() {\n    g(if c {\n        1\n    } else {\n        2\n    });\n}\n",
        "fn f() {\n    g(if c {\n        1\n    } else {\n        2\n    });\n}\n",
    );
    // A break between two items does break it.
    pin(
        "fn f() {\n    g(1,\n        2);\n}\n",
        "fn f() {\n    g(\n        1,\n        2,\n    );\n}\n",
    );
}

/// `else` hangs off the `}` or starts a line, as written. No block's span
/// records this, so it is read off the `else` token.
#[test]
fn the_else_keyword_stays_where_it_was_put() {
    pin(
        "fn f() -> str {\n    if a { \"x\" }\n    else if b { \"y\" }\n    else { \"z\" }\n}\n",
        "fn f() -> str {\n    if a { \"x\" }\n    else if b { \"y\" }\n    else { \"z\" }\n}\n",
    );
    pin(
        "fn f() -> str {\n    if a { \"x\" } else { \"z\" }\n}\n",
        "fn f() -> str {\n    if a { \"x\" } else { \"z\" }\n}\n",
    );
}

/// A block comment inside a group the author kept on one line has no line of
/// its own to sit on, so it moves to the end of that line. It is never dropped
/// — that is the property that matters — but this is the one placement the
/// formatter does not preserve exactly.
#[test]
fn an_inline_block_comment_moves_to_the_end_of_its_line() {
    pin(
        "fn f() { g(/* why */ 3); }\n",
        "fn f() {\n    g(3);  /* why */\n}\n",
    );
    // A line comment can never be in this position: it would end the line.
    pin(
        "fn f() {\n    g(\n        // why\n        3,\n    );\n}\n",
        "fn f() {\n    g(\n        // why\n        3,\n    );\n}\n",
    );
}

#[test]
fn a_semicolon_after_a_block_like_statement_is_the_authors_call() {
    pin(
        "fn f() {\n    if a { g(); } else { h(); }\n    x();\n}\n",
        "fn f() {\n    if a { g(); } else { h(); }\n    x();\n}\n",
    );
    // Dropping this `;` would let `-1` be read as a continuation of the `if`.
    pin(
        "fn f() {\n    if a { g(); } else { h(); };\n    -1;\n}\n",
        "fn f() {\n    if a { g(); } else { h(); };\n    -1;\n}\n",
    );
}

#[test]
fn spreads_and_rests_land_last() {
    pin(
        "fn f() { let a = P{x:1,..base}; let b = [1,2,..rest]; }\n",
        "fn f() { let a = P { x: 1, ..base }; let b = [1, 2, ..rest]; }\n",
    );
    pin(
        "fn f() { match p { P{x,..} => 1, _ => 2 } }\n",
        "fn f() { match p { P { x, .. } => 1, _ => 2 } }\n",
    );
    pin(
        "fn f() {\n    let a = P {\n        x: 1,\n        ..base\n    };\n}\n",
        "fn f() {\n    let a = P {\n        x: 1,\n        ..base\n    };\n}\n",
    );
}

#[test]
fn the_try_triad_and_catch() {
    pin(
        "fn f() { let a = try? g(); let b = try! g(); try { g(); h(); } catch (e) { i(); } }\n",
        "fn f() { let a = try? g(); let b = try! g(); try { g(); h(); } catch (e) { i(); } }\n",
    );
}

#[test]
fn else_if_chains_stay_flat() {
    pin(
        "fn f() {\n    if a {\n        g();\n    } else if b {\n        h();\n    } else {\n        i();\n    }\n}\n",
        "fn f() {\n    if a {\n        g();\n    } else if b {\n        h();\n    } else {\n        i();\n    }\n}\n",
    );
}

#[test]
fn a_whole_file() {
    pin(
        "\
use std::io;

/// Adds two numbers.
fn add(a: i64, b: i64) -> i64 { a + b }

record Point { x: i64, y: i64 }

fn main() {
    let p = Point { x: 1, y: 2 };

    // The interesting part.
    for i in list::range(0, 10) {
        io::println(\"#{i}: #{add(i, p.x)}\");
    }

    let msg = match p.x {
        0 => \"zero\",
        n if n > 0 => \"positive\",
        _ => \"negative\",
    };
    io::println(msg);
}
",
        "\
use std::io;

/// Adds two numbers.
fn add(a: i64, b: i64) -> i64 { a + b }

record Point { x: i64, y: i64 }

fn main() {
    let p = Point { x: 1, y: 2 };

    // The interesting part.
    for i in list::range(0, 10) {
        io::println(\"#{i}: #{add(i, p.x)}\");
    }

    let msg = match p.x {
        0 => \"zero\",
        n if n > 0 => \"positive\",
        _ => \"negative\",
    };
    io::println(msg);
}
",
    );
}

#[test]
fn an_empty_file_formats_to_nothing() {
    pin("", "");
}

#[test]
fn a_thrown_arrow_keeps_its_parentheses() {
    // Dropping them prints `throws (str) -> i64 -> i64`, which does not reparse:
    // the thrown arrow rebinds to the clause's own `->`. `check` runs the
    // round-trip, so this fails on the tree, not on the text.
    let out = check("fn j() throws ((str) -> i64) -> i64 { 0 }");
    assert!(out.contains("throws ((str) -> i64) ->"), "{out}");

    let out = check("type H = (i64) throws ((str) -> i64) -> i64");
    assert!(out.contains("throws ((str) -> i64) ->"), "{out}");
}

#[test]
fn a_thrown_type_that_needs_no_parentheses_gets_none() {
    assert_eq!(check("fn f() throws str -> i64 { 0 }"), "fn f() throws str -> i64 { 0 }\n");
    // A union sits at the throws level, so it needs no grouping either.
    assert_eq!(
        check("fn f() throws :err | :other -> i64 { 0 }"),
        "fn f() throws :err | :other -> i64 { 0 }\n"
    );
}

#[test]
fn a_parenthesised_thrown_type_loses_redundant_parentheses() {
    // `(str)` is a grouping, not a tuple, so the parser never built a node for it
    // and the formatter has nothing to print. The tree is what round-trips.
    assert_eq!(check("fn f() throws (str) -> i64 { 0 }"), "fn f() throws str -> i64 { 0 }\n");
}
