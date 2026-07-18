use super::*;
use crate::{lexer, parser};

fn parse(src: &str) -> Module {
    let tokens = lexer::lex(src).expect("the fixture lexes");
    let (m, errs) = parser::parse(&tokens, src.len());
    assert!(errs.is_empty(), "parse errors in the fixture: {errs:?}");
    m.expect("the fixture parses")
}

fn run(src: &str, config: Config) -> (Module, Meta, Vec<Error>) {
    expand(parse(src), &config)
}

/// The names of the top-level declarations that survived, for asserting on `@cfg`.
fn survivors(m: &Module) -> Vec<String> {
    m.decls
        .iter()
        .filter_map(|d| match &d.kind {
            DeclKind::Fn(f) => Some(f.name.clone()),
            DeclKind::Record(r) => Some(r.name.clone()),
            DeclKind::Mod(md) => Some(md.name.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn an_unknown_annotation_is_an_error() {
    let (_, _, errs) = run("@wat fn f() {}", Config::default());
    assert!(errs.iter().any(|e| e.message.contains("unknown annotation `@wat`")), "{errs:?}");
}

#[test]
fn a_known_annotation_is_not_unknown() {
    let (_, _, errs) = run(r#"@doc("ok") fn f() {}"#, Config::default());
    assert!(errs.is_empty(), "{errs:?}");
}

#[test]
fn native_wants_a_symbol_and_no_body() {
    // Valid: a symbol, no body.
    let (_, _, errs) = run(r#"@native("neon_f") fn f()"#, Config::default());
    assert!(errs.is_empty(), "{errs:?}");
    // A body is a contradiction.
    let (_, _, errs) = run(r#"@native("neon_f") fn f() { }"#, Config::default());
    assert!(errs.iter().any(|e| e.message.contains("no body")), "{errs:?}");
    // The symbol is required.
    let (_, _, errs) = run("@native fn f()", Config::default());
    assert!(errs.iter().any(|e| e.message.contains("runtime symbol")), "{errs:?}");
}

#[test]
fn native_is_only_for_a_fn() {
    let (_, _, errs) = run(r#"@native("x") record R { a: i64 }"#, Config::default());
    assert!(errs.iter().any(|e| e.message.contains("only for a `fn`")), "{errs:?}");
}

#[test]
fn doc_pulls_text_into_metadata_and_keeps_the_node() {
    let (m, meta, errs) = run(r#"@doc("a thing") record Thing { a: i64 }"#, Config::default());
    assert!(errs.is_empty(), "{errs:?}");
    assert_eq!(meta.docs, vec![("Thing".to_string(), "a thing".to_string())]);
    assert_eq!(survivors(&m), vec!["Thing"]);
}

#[test]
fn cfg_keeps_when_the_key_is_active() {
    let cfg = Config::with(["linux".to_string()]);
    let (m, _, errs) = run(r#"@cfg("linux") fn only_linux() {} fn always() {}"#, cfg);
    assert!(errs.is_empty(), "{errs:?}");
    assert_eq!(survivors(&m), vec!["only_linux", "always"]);
}

#[test]
fn cfg_omits_when_the_key_is_inactive() {
    let (m, _, errs) = run(r#"@cfg("windows") fn only_win() {} fn always() {}"#, Config::default());
    assert!(errs.is_empty(), "{errs:?}");
    assert_eq!(survivors(&m), vec!["always"]);
}

#[test]
fn cfg_understands_not_all_and_any() {
    let cfg = Config::with(["linux".to_string(), "x86".to_string()]);
    let keep = |src: &str| survivors(&run(src, cfg.clone()).0).contains(&"f".to_string());
    assert!(keep(r#"@cfg("not(windows)") fn f() {}"#));
    assert!(!keep(r#"@cfg("not(linux)") fn f() {}"#));
    assert!(keep(r#"@cfg("all(linux, x86)") fn f() {}"#));
    assert!(!keep(r#"@cfg("all(linux, arm)") fn f() {}"#));
    assert!(keep(r#"@cfg("any(windows, x86)") fn f() {}"#));
    assert!(!keep(r#"@cfg("any(windows, arm)") fn f() {}"#));
    assert!(keep(r#"@cfg("all(linux, any(x86, arm))") fn f() {}"#));
}

#[test]
fn a_malformed_cfg_condition_is_an_error_not_a_silent_drop() {
    let (m, _, errs) = run(r#"@cfg("all(linux") fn f() {}"#, Config::default());
    assert!(errs.iter().any(|e| e.message.contains("`@cfg`")), "{errs:?}");
    // Conservative: on a bad condition the node is kept, not silently dropped.
    assert_eq!(survivors(&m), vec!["f"]);
}

#[test]
fn cfg_reaches_methods_and_nested_mods() {
    // A method dropped by cfg.
    let src = r#"
        protocol P for T {
            @cfg("windows") fn win(v: T) -> i64
            fn common(v: T) -> i64
        }
        mod inner {
            @cfg("windows") fn win() {}
            fn keep() {}
        }
    "#;
    let (m, _, errs) = run(src, Config::default());
    assert!(errs.is_empty(), "{errs:?}");
    // The protocol keeps only `common`.
    let proto = m.decls.iter().find_map(|d| match &d.kind {
        DeclKind::Protocol(p) => Some(p),
        _ => None,
    });
    let methods: Vec<_> = proto.unwrap().methods.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(methods, vec!["common"]);
    // The mod keeps only `keep`.
    let inner = m.decls.iter().find_map(|d| match &d.kind {
        DeclKind::Mod(md) => Some(md),
        _ => None,
    });
    let inner_names: Vec<_> = inner.unwrap().decls.iter().filter_map(|d| match &d.kind {
        DeclKind::Fn(f) => Some(f.name.as_str()),
        _ => None,
    }).collect();
    assert_eq!(inner_names, vec!["keep"]);
}
