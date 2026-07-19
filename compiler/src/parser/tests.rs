use super::*;
use crate::lexer;

fn parse_src(src: &str) -> (Option<Module>, Vec<ParseError>) {
    let tokens = lexer::lex(src).expect("lexes");
    parse(&tokens, src.len())
}

fn ok(src: &str) -> Module {
    let (m, errs) = parse_src(src);
    assert!(errs.is_empty(), "unexpected errors in {src:?}: {errs:?}");
    m.expect("parses")
}

fn errs(src: &str) -> Vec<ParseError> {
    let (_, errs) = parse_src(src);
    assert!(!errs.is_empty(), "expected errors from {src:?}, got none");
    errs
}

/// The body of the first fn.
fn body(src: &str) -> Block {
    let m = ok(src);
    match &m.decls[0].kind {
        DeclKind::Fn(f) => f.body.clone().expect("a body"),
        other => panic!("expected a fn, got {other:?}"),
    }
}

/// The tail expression of `fn main() { <expr> }`.
fn tail(expr_src: &str) -> Expr {
    let b = body(&format!("fn main() {{ {expr_src} }}"));
    *b.tail.expect("a tail expression")
}

fn binop(e: &Expr) -> (BinOp, &Expr, &Expr) {
    match &e.kind {
        ExprKind::Binary { op, lhs, rhs } => (*op, lhs, rhs),
        other => panic!("expected a binary op, got {other:?}"),
    }
}

// ---- declarations ----

#[test]
fn the_vertical_slice() {
    let m = ok("fn main() {}");
    match &m.decls[0].kind {
        DeclKind::Fn(f) => {
            assert_eq!(f.name, "main");
            assert!(f.params.is_empty() && f.ret.is_none() && f.throws.is_none());
        }
        other => panic!("expected a fn, got {other:?}"),
    }
}

#[test]
fn an_arrow_type_carries_a_throws() {
    let m = ok("type H = (i64) throws :error -> i64");
    match &m.decls[0].kind {
        DeclKind::TypeAlias(a) => match &a.value.kind {
            TypeSpecKind::Fn { params, throws, ret } => {
                assert_eq!(params.len(), 1);
                assert_eq!(throws.as_ref().expect("throws").kind, TypeSpecKind::Atom("error".into()));
                assert!(matches!(ret.kind, TypeSpecKind::Named { .. }));
            }
            other => panic!("expected a fn type, got {other:?}"),
        },
        other => panic!("expected a type alias, got {other:?}"),
    }
}

#[test]
fn an_arrow_type_without_a_throws_has_none() {
    let m = ok("type H = (i64) -> i64");
    match &m.decls[0].kind {
        DeclKind::TypeAlias(a) => match &a.value.kind {
            TypeSpecKind::Fn { throws, .. } => assert!(throws.is_none()),
            other => panic!("expected a fn type, got {other:?}"),
        },
        other => panic!("expected a type alias, got {other:?}"),
    }
}

/// `throws` binds to the `->` that follows it. Without one there is no arrow to
/// attach to, and the clause must not be silently swallowed by a tuple.
#[test]
fn a_throws_with_no_arrow_is_rejected() {
    errs("type H = (i64) throws :error");
}

#[test]
fn a_parenthesised_throws_is_not_the_return() {
    // `throws` parses below the arrow, so `(str)` is the thrown type and `i64` the
    // return. Parsed at the full type level, `(str) -> i64` would be read as the
    // thrown type and the return would silently vanish.
    let m = ok("type H = (i64) throws (str) -> i64");
    let DeclKind::TypeAlias(a) = &m.decls[0].kind else { panic!("a type alias") };
    let TypeSpecKind::Fn { params, throws, ret } = &a.value.kind else { panic!("an arrow") };
    assert_eq!(params.len(), 1);
    assert!(matches!(&throws.as_deref().expect("a throws").kind,
        TypeSpecKind::Named { path, .. } if path == &["str"]));
    assert!(matches!(&ret.kind, TypeSpecKind::Named { path, .. } if path == &["i64"]));
}

#[test]
fn a_fn_decl_throws_does_not_swallow_its_return() {
    // The same ambiguity in declaration position, where it used to misparse in
    // silence: `throws=((str) -> i64), ret=None`.
    let m = ok("fn f() throws (str) -> i64 { 0 }");
    let DeclKind::Fn(f) = &m.decls[0].kind else { panic!("a fn") };
    assert!(matches!(&f.throws.as_ref().expect("a throws").kind,
        TypeSpecKind::Named { path, .. } if path == &["str"]));
    assert!(matches!(&f.ret.as_ref().expect("a return").kind,
        TypeSpecKind::Named { path, .. } if path == &["i64"]));
}

#[test]
fn a_thrown_arrow_needs_its_own_parens() {
    // The restriction is only on a TOP-LEVEL arrow, so an arrow is still thrown by
    // grouping it or putting it under a constructor.
    ok("fn f() throws ((str) -> i64) -> i64 { 0 }");
    ok("fn g() throws Handler[(i64) -> i64] -> i64 { 0 }");
}

#[test]
fn a_throws_union_still_parses() {
    ok("fn f() throws :err | :other -> i64 { 0 }");
    ok("type H = (i64) throws :err | :other -> i64");
}

#[test]
fn throws_comes_before_the_return_type() {
    let m = ok("fn get[T](xs: List[T], i: i64) throws IndexError -> T { xs }");
    match &m.decls[0].kind {
        DeclKind::Fn(f) => {
            assert_eq!(f.generics, vec!["T"]);
            assert_eq!(f.params.len(), 2);
            assert!(f.throws.is_some() && f.ret.is_some());
        }
        other => panic!("expected a fn, got {other:?}"),
    }
}

#[test]
fn where_clauses_and_annotations() {
    let m = ok(r#"@native("neon_x") fn f[T](a: T) -> T where T: Display { a }"#);
    match &m.decls[0].kind {
        DeclKind::Fn(f) => {
            assert_eq!(f.annotations.len(), 1);
            assert_eq!(f.annotations[0].name, "native");
            assert_eq!(f.annotations[0].arg.as_deref(), Some("neon_x"));
            assert_eq!(f.wheres.len(), 1);
            assert_eq!(f.wheres[0].param, "T");
        }
        other => panic!("expected a fn, got {other:?}"),
    }
}

#[test]
fn records_including_opaque_and_unit() {
    let m = ok("record Point { x: i64, y: i64 } opaque record Rng { seed: i64 } record Red {}");
    assert_eq!(m.decls.len(), 3);
    match (&m.decls[0].kind, &m.decls[1].kind, &m.decls[2].kind) {
        (DeclKind::Record(p), DeclKind::Record(r), DeclKind::Record(red)) => {
            assert_eq!(p.fields.len(), 2);
            assert!(!p.opaque);
            assert!(r.opaque, "opaque should be recorded");
            // Unit records are how sum-type variants are written.
            assert!(red.fields.is_empty());
        }
        other => panic!("expected records, got {other:?}"),
    }
}

#[test]
fn the_three_type_declaration_forms() {
    let m = ok("type A = i64 mu type J = :ok | List[J] newtype UserId = str");
    assert_eq!(m.decls.len(), 3);
    assert!(matches!(m.decls[0].kind, DeclKind::TypeAlias(_)));
    assert!(matches!(m.decls[1].kind, DeclKind::MuType(_)));
    assert!(matches!(m.decls[2].kind, DeclKind::Newtype(_)));
}

#[test]
fn the_mu_type_from_the_spec() {
    let m = ok("mu type A = :ok | List[A]");
    match &m.decls[0].kind {
        DeclKind::MuType(a) => {
            assert_eq!(a.name, "A");
            match &a.value.kind {
                TypeSpecKind::Union(parts) => {
                    assert_eq!(parts.len(), 2);
                    assert_eq!(parts[0].kind, TypeSpecKind::Atom("ok".into()));
                }
                other => panic!("expected a union, got {other:?}"),
            }
        }
        other => panic!("expected a mu type, got {other:?}"),
    }
}

#[test]
fn protocols_and_impls() {
    let m = ok(
        "protocol Sized for T { fn len(v: T) -> i64 } \
         impl Sized for str { fn len(v: str) -> i64 { 0 } }",
    );
    match (&m.decls[0].kind, &m.decls[1].kind) {
        (DeclKind::Protocol(p), DeclKind::Impl(i)) => {
            assert_eq!(p.name, "Sized");
            assert_eq!(p.subject, "T");
            // A protocol method is a signature with no body.
            assert!(p.methods[0].body.is_none());
            assert_eq!(i.protocol, vec!["Sized"]);
            assert!(i.methods[0].body.is_some());
        }
        other => panic!("expected protocol + impl, got {other:?}"),
    }
}

#[test]
fn use_mod_const() {
    let m = ok("use std::io; internal mod helpers { fn h() {} } const PI: f64 = 3.14;");
    assert!(matches!(m.decls[0].kind, DeclKind::Use(_)));
    match &m.decls[1].kind {
        DeclKind::Mod(md) => {
            assert!(md.internal);
            assert_eq!(md.decls.len(), 1);
        }
        other => panic!("expected a mod, got {other:?}"),
    }
    assert!(matches!(m.decls[2].kind, DeclKind::Const(_)));
}

#[test]
fn test_and_bench_blocks() {
    let m = ok(r#"test "adds two" { assert_eq(1, 1); } bench "push" {}"#);
    match (&m.decls[0].kind, &m.decls[1].kind) {
        (DeclKind::TestBlock(t), DeclKind::TestBlock(b)) => {
            assert_eq!(t.kind, TestKind::Test);
            assert_eq!(t.name, "adds two");
            assert_eq!(b.kind, TestKind::Bench);
        }
        other => panic!("expected test blocks, got {other:?}"),
    }
}

// ---- types ----

#[test]
fn union_intersection_negation_and_structural() {
    let m = ok("type T = { name: str } & !null | List[i64]");
    match &m.decls[0].kind {
        // `!` tightest, then `&`, then `|`: a union of intersections.
        DeclKind::TypeAlias(a) => match &a.value.kind {
            TypeSpecKind::Union(parts) => {
                assert_eq!(parts.len(), 2);
                assert!(matches!(parts[0].kind, TypeSpecKind::Intersect(_)));
            }
            other => panic!("expected a union at the top, got {other:?}"),
        },
        other => panic!("expected an alias, got {other:?}"),
    }
}

#[test]
fn function_and_tuple_types() {
    let m = ok("type F = (i64, str) -> bool type P = (i64, str) type G = (i64)");
    match &m.decls[0].kind {
        DeclKind::TypeAlias(a) => assert!(matches!(a.value.kind, TypeSpecKind::Fn { .. })),
        other => panic!("expected an alias, got {other:?}"),
    }
    match &m.decls[1].kind {
        DeclKind::TypeAlias(a) => assert!(matches!(a.value.kind, TypeSpecKind::Tuple(_))),
        other => panic!("expected an alias, got {other:?}"),
    }
    // `(A)` is a grouping, not a 1-tuple.
    match &m.decls[2].kind {
        DeclKind::TypeAlias(a) => assert!(matches!(a.value.kind, TypeSpecKind::Named { .. })),
        other => panic!("expected an alias, got {other:?}"),
    }
}

// ---- precedence: the decided ladder ----

#[test]
fn and_binds_tighter_than_or() {
    let e = tail("a or b and c"); // a or (b and c)
    let (op, _, rhs) = binop(&e);
    assert_eq!(op, BinOp::Or);
    assert_eq!(binop(rhs).0, BinOp::And);
}

#[test]
fn pipe_binds_tighter_than_comparison() {
    // (x |> f()) == 3 — piping into a comparison could never be a valid target.
    let e = tail("x |> f() == 3");
    let (op, lhs, _) = binop(&e);
    assert_eq!(op, BinOp::Eq);
    assert_eq!(binop(lhs).0, BinOp::Pipe);
}

#[test]
fn orelse_is_the_loosest() {
    let e = tail("a or b orelse c"); // (a or b) orelse c
    let (op, lhs, _) = binop(&e);
    assert_eq!(op, BinOp::Orelse);
    assert_eq!(binop(lhs).0, BinOp::Or);
}

#[test]
fn arithmetic_precedence() {
    let e = tail("1 + 2 * 3");
    let (op, _, rhs) = binop(&e);
    assert_eq!(op, BinOp::Add);
    assert_eq!(binop(rhs).0, BinOp::Mul);
}

#[test]
fn comparison_binds_tighter_than_and() {
    let e = tail("a < b and c > d");
    let (op, lhs, rhs) = binop(&e);
    assert_eq!(op, BinOp::And);
    assert_eq!(binop(lhs).0, BinOp::Lt);
    assert_eq!(binop(rhs).0, BinOp::Gt);
}

#[test]
fn left_associativity() {
    let e = tail("1 - 2 - 3"); // (1 - 2) - 3
    let (op, lhs, _) = binop(&e);
    assert_eq!(op, BinOp::Sub);
    assert_eq!(binop(lhs).0, BinOp::Sub);
}

// ---- expressions ----

#[test]
fn i64_min_survives_the_parser() {
    // The literal is the magnitude; `-` is a unary op applied to it. An i64
    // literal could not hold 9223372036854775808 at all.
    let e = tail("-9223372036854775808");
    match &e.kind {
        ExprKind::Unary { op: UnOp::Neg, rhs } => {
            assert_eq!(rhs.kind, ExprKind::Int(9223372036854775808));
        }
        other => panic!("expected a negation, got {other:?}"),
    }
}

#[test]
fn calls_indexes_fields_and_turbofish() {
    // Postfix chains left to right.
    match &tail("a.b(1)[2].c").kind {
        ExprKind::Field { name, .. } => assert_eq!(name, "c"),
        other => panic!("expected a field access, got {other:?}"),
    }
    match &tail("list::new[i64]()").kind {
        ExprKind::Call { generics, .. } => assert_eq!(generics.len(), 1),
        other => panic!("expected a call, got {other:?}"),
    }
}

#[test]
fn string_interpolation_reassembles() {
    match &tail(r#""a #{x} b""#).kind {
        ExprKind::Str(parts) => {
            assert_eq!(parts.len(), 3);
            assert_eq!(parts[0], StrPart::Text("a ".into()));
            assert!(matches!(parts[1], StrPart::Interp(_)));
            assert_eq!(parts[2], StrPart::Text(" b".into()));
        }
        other => panic!("expected a string, got {other:?}"),
    }
}

#[test]
fn a_record_literal_inside_an_interpolation() {
    // The case that made `{}` delimiters ambiguous and forced `#{}`.
    match &tail(r##""#{Point { x: 1 }}""##).kind {
        ExprKind::Str(parts) => match &parts[0] {
            StrPart::Interp(inner) => assert!(matches!(inner.kind, ExprKind::RecordLit { .. })),
            other => panic!("expected an interpolation, got {other:?}"),
        },
        other => panic!("expected a string, got {other:?}"),
    }
}

#[test]
fn the_try_triad() {
    assert!(matches!(
        tail("try f()").kind,
        ExprKind::Try { form: TryForm::Propagate, catch: None, .. }
    ));
    assert!(matches!(tail("try? f()").kind, ExprKind::Try { form: TryForm::Soften, .. }));
    assert!(matches!(tail("try! f()").kind, ExprKind::Try { form: TryForm::Assert, .. }));
}

#[test]
fn try_catch_and_the_block_form() {
    match tail("try f() catch (e) { 0 }").kind {
        ExprKind::Try { form: TryForm::Propagate, catch: Some(c), .. } => {
            assert_eq!(c.binding, "e")
        }
        other => panic!("expected try/catch, got {other:?}"),
    }
    // All forms accept a block, so every throwing call inside is covered.
    match tail("try { a(); b() } catch (e) { 0 }").kind {
        ExprKind::Try { body, catch: Some(_), .. } => {
            assert!(matches!(body.kind, ExprKind::Block(_)))
        }
        other => panic!("expected a block try, got {other:?}"),
    }
}

#[test]
fn try_bang_is_not_a_negation() {
    // The old parser read `try! f()` as `try (!f())`, and the mistake went
    // unnoticed because it still parsed.
    match tail("try! f()").kind {
        ExprKind::Try { form, body, .. } => {
            assert_eq!(form, TryForm::Assert);
            assert!(
                matches!(body.kind, ExprKind::Call { .. }),
                "the body should be the call itself, not a negation of it"
            );
        }
        other => panic!("expected try!, got {other:?}"),
    }
}

#[test]
fn if_else_chain() {
    match tail("if a { 1 } else if b { 2 } else { 3 }").kind {
        ExprKind::If { else_: Some(e), .. } => {
            assert!(matches!(e.kind, ExprKind::If { .. }), "else-if nests")
        }
        other => panic!("expected an if, got {other:?}"),
    }
    // The parser records a missing else rather than substituting null; whether
    // that is an error depends on the position, which a later pass decides.
    assert!(matches!(tail("if a { 1 }").kind, ExprKind::If { else_: None, .. }));
}

#[test]
fn match_with_guards_and_patterns() {
    match &tail("match s { is Circle if r > 0 => 1, Point { x, y } => 2, :ok => 3, _ => 4 }").kind {
        ExprKind::Match { arms, .. } => {
            assert_eq!(arms.len(), 4);
            assert!(arms[0].guard.is_some(), "guards are parsed");
            assert!(matches!(arms[0].pat.kind, PatternKind::Is(_)));
            match &arms[1].pat.kind {
                // `{ x, y }` shorthand binds each field to its own name.
                PatternKind::Record { fields, .. } => {
                    assert_eq!(fields.len(), 2);
                    assert!(fields[0].pat.is_none(), "shorthand has no sub-pattern");
                }
                other => panic!("expected a record pattern, got {other:?}"),
            }
            assert!(matches!(arms[3].pat.kind, PatternKind::Wildcard));
        }
        other => panic!("expected a match, got {other:?}"),
    }
}

#[test]
fn loops() {
    assert!(matches!(tail("loop { break 1 }").kind, ExprKind::Loop { .. }));
    assert!(matches!(tail("while a { }").kind, ExprKind::While { .. }));
    assert!(matches!(tail("for x in xs { }").kind, ExprKind::For { .. }));
}

#[test]
fn lambdas_and_pipes() {
    match tail("(x: i64) => x + 1").kind {
        ExprKind::Lambda { params, .. } => {
            assert_eq!(params[0].name, "x");
            assert!(params[0].ty.is_some());
        }
        other => panic!("expected a lambda, got {other:?}"),
    }
    assert_eq!(binop(&tail("xs |> len()")).0, BinOp::Pipe);
}

#[test]
fn lists_and_spread() {
    match tail("[1, ..rest, 3]").kind {
        ExprKind::List(elems) => {
            assert_eq!(elems.len(), 3);
            assert!(matches!(elems[1], Elem::Spread(_)));
        }
        other => panic!("expected a list, got {other:?}"),
    }
}

#[test]
fn record_literal_with_spread() {
    match tail("Point { x: 1, ..base }").kind {
        ExprKind::RecordLit { path, fields, spread } => {
            assert_eq!(path.as_deref(), Some(&["Point".to_string()][..]));
            assert_eq!(fields.len(), 1);
            assert!(spread.is_some());
        }
        other => panic!("expected a record literal, got {other:?}"),
    }
    // The bare form: an anonymous record, which is how optional params arrive.
    match tail("{ timeout: 5 }").kind {
        ExprKind::RecordLit { path, fields, .. } => {
            // None, not an empty vec: anonymity is stated, not encoded in a
            // length check the reader has to know about.
            assert!(path.is_none());
            assert_eq!(fields.len(), 1);
        }
        other => panic!("expected an anonymous record, got {other:?}"),
    }
}

#[test]
fn is_and_as() {
    assert!(matches!(tail("x is Circle").kind, ExprKind::Is { .. }));
    assert!(matches!(tail("x as Circle").kind, ExprKind::As { .. }));
}

#[test]
fn assert_intrinsics() {
    match tail("assert_eq(a, b)").kind {
        ExprKind::Assert { kind, args } => {
            assert_eq!(kind, AssertKind::Eq);
            assert_eq!(args.len(), 2);
        }
        other => panic!("expected an assert, got {other:?}"),
    }
}

// ---- statements ----

#[test]
fn let_and_rebind() {
    let b = body("fn main() { let x = 1; x = 2; }");
    assert_eq!(b.stmts.len(), 2);
    assert!(matches!(b.stmts[0].kind, StmtKind::Let { .. }));
    match &b.stmts[1].kind {
        // Bindings rebind; there is no `mut`.
        StmtKind::Assign { name, .. } => assert_eq!(name, "x"),
        other => panic!("expected an assign, got {other:?}"),
    }
}

#[test]
fn a_block_tail_is_its_value() {
    let b = body("fn main() { let x = 1; x }");
    assert_eq!(b.stmts.len(), 1);
    assert!(b.tail.is_some(), "the trailing expression is the block's value");
}

#[test]
fn destructuring_let() {
    let b = body("fn main() { let Point { x, y } = p; }");
    match &b.stmts[0].kind {
        StmtKind::Let { pat, .. } => assert!(matches!(pat.kind, PatternKind::Record { .. })),
        other => panic!("expected a let, got {other:?}"),
    }
}

// ---- diagnostics ----

#[test]
fn field_assignment_is_rejected_with_advice() {
    // Records are values. People will write this, so it gets a diagnostic
    // rather than a parse failure.
    let e = errs("fn main() { p.x = 1; }");
    let found = e
        .iter()
        .find(|e| e.kind == ParseErrorKind::FieldAssignment)
        .unwrap_or_else(|| panic!("expected the field-assignment diagnostic, got {e:?}"));
    assert!(found.to_string().contains("..p"), "should show the spread form: {found}");
}

#[test]
fn index_assignment_is_rejected_with_advice() {
    let e = errs("fn main() { xs[0] = 1; }");
    let found = e
        .iter()
        .find(|e| e.kind == ParseErrorKind::IndexAssignment)
        .unwrap_or_else(|| panic!("expected the index-assignment diagnostic, got {e:?}"));
    assert!(
        found.to_string().contains("list::set"),
        "should show the returned-copy form: {found}"
    );
}

#[test]
fn enum_gets_a_real_diagnostic() {
    let e = errs("enum Color { Red, Green }");
    let found = e
        .iter()
        .find(|e| e.kind == ParseErrorKind::EnumDeclaration)
        .unwrap_or_else(|| panic!("expected the enum diagnostic, got {e:?}"));
    assert!(
        found.to_string().contains("record Red"),
        "should say what to write instead: {found}"
    );
}

#[test]
fn recovery_keeps_going_after_a_bad_decl() {
    let (m, errs) = parse_src("fn broken( {} fn good() {} fn also_good() {}");
    assert!(!errs.is_empty());
    let names: Vec<_> = m
        .expect("recovery still yields a module")
        .decls
        .iter()
        .filter_map(|d| match &d.kind {
            DeclKind::Fn(f) => Some(f.name.clone()),
            _ => None,
        })
        .collect();
    assert!(
        names.contains(&"good".to_string()) && names.contains(&"also_good".to_string()),
        "later decls should still parse, got {names:?}"
    );
}

#[test]
fn every_error_is_reported_not_just_the_first() {
    let e = errs("fn a( {} fn b( {} fn c() {}");
    assert!(e.len() >= 2, "expected several errors, got {}: {e:?}", e.len());
}

#[test]
fn errors_are_concrete_and_carry_a_span() {
    let e = errs("fn 123() {}");
    match &e[0].kind {
        ParseErrorKind::Expected { expected, found } => {
            assert!(expected.contains(&Expected::Label("an identifier")), "{expected:?}");
            assert_eq!(found, &Some(Token::Int(123)));
        }
        other => panic!("expected an Expected error, got {other:?}"),
    }
    assert!(e[0].span.start < e[0].span.end);
}

#[test]
fn a_label_names_the_construct() {
    let e = errs("fn f(x: ) {}");
    let msg = e[0].to_string();
    assert!(msg.contains("a type"), "should name the construct: {msg}");
}

#[test]
fn empty_input_is_an_empty_module() {
    assert!(ok("").decls.is_empty());
}

#[test]
fn annotations_take_an_optional_string() {
    // One shape for all of them: `@name` or `@name("...")`. `@cfg` takes a
    // string rather than a nested expression (`@cfg("not(windows)")`, not
    // `@cfg(not(windows))`), so the grammar needs no expression language of its
    // own; whatever evaluates the cfg parses its contents.
    let m = ok(r#"@native("neon_x") @cfg("not(windows)") @doc("Adds two numbers.") fn f() {}"#);
    match &m.decls[0].kind {
        DeclKind::Fn(f) => {
            let names: Vec<_> = f.annotations.iter().map(|a| a.name.as_str()).collect();
            assert_eq!(names, ["native", "cfg", "doc"]);
            assert_eq!(f.annotations[1].arg.as_deref(), Some("not(windows)"));
            assert_eq!(f.annotations[2].arg.as_deref(), Some("Adds two numbers."));
        }
        other => panic!("expected a fn, got {other:?}"),
    }
}

#[test]
fn an_annotation_may_have_no_argument() {
    let m = ok("@inline fn f() {}");
    match &m.decls[0].kind {
        DeclKind::Fn(f) => {
            assert_eq!(f.annotations[0].name, "inline");
            assert!(f.annotations[0].arg.is_none());
        }
        other => panic!("expected a fn, got {other:?}"),
    }
}

#[test]
fn annotations_on_records() {
    let m = ok(r#"@native("NeonBytes*") opaque record Bytes {}"#);
    match &m.decls[0].kind {
        DeclKind::Record(r) => {
            assert!(r.opaque);
            assert_eq!(r.annotations[0].arg.as_deref(), Some("NeonBytes*"));
        }
        other => panic!("expected a record, got {other:?}"),
    }
}

#[test]
fn a_qualified_name_cannot_be_rebound() {
    // Only a binding can be rebound, so the AST holds a String rather than a
    // path. The parser rejects the rest rather than handing a later pass
    // something it could not act on.
    let e = errs("fn main() { a::b = 1; }");
    assert!(
        e.iter().any(|e| e.kind == ParseErrorKind::InvalidAssignTarget),
        "expected the invalid-target diagnostic, got {e:?}"
    );
}

#[test]
fn try_binds_tighter_than_binary_operators() {
    // `try? get(m, k) orelse 30` is the documented easy path. With `try`'s body
    // as the full expression parser it read as `try? (get(m, k) orelse 30)` —
    // an orelse on a non-nullable type, which is a no-op by the language's own
    // rule, so the default silently never applied and the result stayed
    // `i64 | null`. A wrong answer with no error.
    let e = tail("try? get(m, k) orelse 30");
    let (op, lhs, _) = binop(&e);
    assert_eq!(op, BinOp::Orelse);
    assert!(
        matches!(lhs.kind, ExprKind::Try { form: TryForm::Soften, .. }),
        "the orelse must apply to the try's result, got {:?}",
        lhs.kind
    );
}

#[test]
fn postfix_binds_tighter_than_prefix() {
    // `-x.f` is `-(x.f)`. Folding the prefix operators before postfix ran gave
    // `(-x).f`, which is what C and Rust do not do.
    match tail("-x.f").kind {
        ExprKind::Unary { op: UnOp::Neg, rhs } => {
            assert!(matches!(rhs.kind, ExprKind::Field { .. }), "got {:?}", rhs.kind)
        }
        other => panic!("expected a negation of a field access, got {other:?}"),
    }
    match tail("-xs[0]").kind {
        ExprKind::Unary { op: UnOp::Neg, rhs } => {
            assert!(matches!(rhs.kind, ExprKind::Index { .. }), "got {:?}", rhs.kind)
        }
        other => panic!("expected a negation of an index, got {other:?}"),
    }
}

#[test]
fn a_native_fn_has_no_body() {
    // The signature is the declaration; the implementation is in the runtime.
    let m = ok(r#"@native("neon_str_len") fn len(s: str) -> i64"#);
    match &m.decls[0].kind {
        DeclKind::Fn(f) => {
            assert_eq!(f.annotations[0].name, "native");
            assert!(f.body.is_none());
            assert!(f.ret.is_some());
        }
        other => panic!("expected a fn, got {other:?}"),
    }
}

#[test]
fn a_block_like_expression_at_statement_start_is_a_statement() {
    // `if a { } else { }` followed by a line beginning `-1;` is two statements.
    // As a binary operand the `if` would swallow the next line and it would
    // silently vanish. Same rule as Rust.
    let b = body("fn main() { if a { g() } else { h() }\n -1; }");
    assert_eq!(b.stmts.len(), 2, "expected two statements, got {:?}", b.stmts);
    match &b.stmts[0].kind {
        StmtKind::Expr(e) => assert!(matches!(e.kind, ExprKind::If { .. })),
        other => panic!("expected the if as a statement, got {other:?}"),
    }
    match &b.stmts[1].kind {
        StmtKind::Expr(e) => assert!(matches!(e.kind, ExprKind::Unary { op: UnOp::Neg, .. })),
        other => panic!("expected the negation as its own statement, got {other:?}"),
    }
}

#[test]
fn a_block_like_expression_is_still_a_value_where_one_is_expected() {
    // Only statement position is special: after `=` it is an ordinary operand.
    let b = body("fn main() { let x = if a { 1 } else { 2 } + 3; }");
    match &b.stmts[0].kind {
        StmtKind::Let { value, .. } => {
            assert_eq!(binop(value).0, BinOp::Add, "the if is the left operand here")
        }
        other => panic!("expected a let, got {other:?}"),
    }
    // And a trailing block-like expression is still the block's value.
    let b = body("fn main() { if a { 1 } else { 2 } }");
    assert!(b.tail.is_some(), "the trailing if is the block's value");
}

#[test]
fn there_is_no_one_element_tuple() {
    // Silently discarding the comma is what this replaces: `(str,)` used to parse
    // as plain `str`, so the type you wrote was not the type you got.
    for src in [
        "type A = (str,)",
        "fn f() { let a = (1,); }",
        "fn f() { match p { (a,) => 1, _ => 0 } }",
    ] {
        let e = errs(src);
        assert!(
            e.iter().any(|e| matches!(e.kind, ParseErrorKind::OneElementTuple)),
            "{src} -- got {e:?}"
        );
    }
}

#[test]
fn a_trailing_comma_is_insignificant_everywhere_it_is_allowed() {
    // The whole reason `(x,)` is rejected rather than meaningful: a trailing comma
    // means nothing in any other list, so it does not get to change a type here.
    ok("type C = (str, i64,)");
    ok("fn f(a: i64,) -> i64 { 0 }");
    ok("fn f() -> i64 { g(1, 2,) }");
    ok("fn f() -> i64 { let xs = [1, 2,]; 0 }");
    ok("record R { a: i64, b: str, }");
    ok("type S = { a: i64, b: str, }");
}

#[test]
fn a_trailing_comma_in_an_arrow_parameter_list_is_fine() {
    // `(A,) -> B` is a parameter list, not a tuple, so the comma means nothing.
    let m = ok("type D = (i64,) -> str");
    let DeclKind::TypeAlias(a) = &m.decls[0].kind else { panic!("a type alias") };
    let TypeSpecKind::Fn { params, .. } = &a.value.kind else { panic!("an arrow") };
    assert_eq!(params.len(), 1);
}

#[test]
fn parens_of_one_are_a_grouping_not_a_tuple() {
    let m = ok("type B = (str)");
    let DeclKind::TypeAlias(a) = &m.decls[0].kind else { panic!("a type alias") };
    assert!(matches!(&a.value.kind, TypeSpecKind::Named { path, .. } if path == &["str"]));

    // Patterns used to disagree with types here: `(x)` built a 1-tuple pattern
    // while `(i64)` was a grouping.
    ok("fn f() { match p { (a) => 1, _ => 0 } }");
}

#[test]
fn unit_and_larger_tuples_are_unaffected() {
    ok("type E = ()");
    let m = ok("type F = (i64, str)");
    let DeclKind::TypeAlias(a) = &m.decls[0].kind else { panic!("a type alias") };
    let TypeSpecKind::Tuple(v) = &a.value.kind else { panic!("a tuple") };
    assert_eq!(v.len(), 2);
}

#[test]
fn use_trees_in_every_shape() {
    for src in [
        "use x::y::z;",
        "use x::y as w;",
        "use x::*;",
        "use x::{a, b as c};",
        "use std::collections::{list, map};",
        "use thing::Frobulate::frobulate;",
        "use a::{b::{c, d}, e::*};",
    ] {
        let m = ok(src);
        assert!(matches!(m.decls[0].kind, DeclKind::Use(_)), "{src}");
    }
}

#[test]
fn a_use_group_flattens_its_prefix() {
    let m = ok("use x::{a, b as c};");
    let DeclKind::Use(u) = &m.decls[0].kind else { panic!() };
    let UseTree::Group { prefix, children } = &u.tree else { panic!("a group") };
    assert_eq!(prefix, &["x"]);
    assert_eq!(children.len(), 2);
    assert!(matches!(&children[1], UseTree::Leaf { alias: Some(a), .. } if a == "c"));
}

#[test]
fn a_protocol_may_carry_a_where_clause() {
    let m = ok("protocol Ord for T where T: Eq { fn cmp(a: T, b: T) -> i64 }");
    let DeclKind::Protocol(p) = &m.decls[0].kind else { panic!() };
    assert_eq!(p.wheres.len(), 1);
    assert_eq!(p.wheres[0].param, "T");
}

/// The grammar is built once per thread and reused, not rebuilt per `parse`.
///
/// This is a leak test wearing a cheaper disguise. `Recursive::declare` hands
/// out a strong `Rc`, and the mutually recursive expression grammar defines
/// `expr`/`cond`/`block`/`unary` in terms of those handles, so the finished
/// graph contains strong cycles and dropping it frees nothing. Rebuilding it
/// per call therefore leaked ~25 kB per call — invisible to the batch compiler,
/// fatal to the language server, which reparses on every edit for the life of a
/// session. Counting constructions pins that exactly, with no dependence on
/// RSS, the allocator, or the platform.
#[test]
fn the_grammar_is_built_once_per_thread() {
    // Warm the thread_local, so the count under test excludes first use.
    let _ = parse_src("fn f() { }");
    let after_warmup = GRAPH_BUILDS.with(|n| n.get());
    assert_eq!(after_warmup, 1, "the grammar should be built exactly once");

    for i in 0..200 {
        let src = format!("fn f{i}(x: int) -> int {{ let y = x + {i} * 2; if y > 0 {{ y }} else {{ -y }} }}");
        let _ = parse_src(&src);
    }
    // `format` goes through `parse` too, so it must share the one graph.
    for i in 0..200 {
        let _ = crate::format::format(&format!("fn g{i}() {{ let a = [1, 2, {i}]; }}"));
    }

    assert_eq!(
        GRAPH_BUILDS.with(|n| n.get()),
        after_warmup,
        "the grammar was rebuilt; every rebuild is a permanent ~25 kB leak, \
         because its Rc cycles make it unreclaimable"
    );
}
