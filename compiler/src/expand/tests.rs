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

// ---- @derive ----

/// The generated impls, as `(protocol, target head, generics, where params)`.
fn derived(m: &Module) -> Vec<(String, String, Vec<String>, Vec<String>)> {
    m.decls
        .iter()
        .filter_map(|d| match &d.kind {
            DeclKind::Impl(i) => {
                let head = match &i.target.kind {
                    crate::ast::TypeSpecKind::Named { path, .. } => path.join("::"),
                    _ => String::new(),
                };
                Some((
                    i.protocol.join("::"),
                    head,
                    i.generics.clone(),
                    i.wheres.iter().map(|w| w.param.clone()).collect(),
                ))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn derive_generates_an_impl_for_the_record() {
    let (m, _, errs) = run(r#"@derive("Display") record P { x: i64 }"#, Config::default());
    assert!(errs.is_empty(), "{errs:?}");
    assert_eq!(derived(&m), vec![("Display".into(), "P".into(), vec![], vec![])]);
    // The record itself survives: a derive adds, it does not replace.
    assert_eq!(survivors(&m), vec!["P".to_string()]);
}

/// A generic record derives a BOUNDED impl. Without the bound the generated body could not
/// call `to_string` on a field of type `T` at all, because `T` is rigid inside the impl.
#[test]
fn deriving_a_generic_record_bounds_each_parameter() {
    let (m, _, errs) =
        run(r#"@derive("Display") record Box[T, U] { a: T, b: U }"#, Config::default());
    assert!(errs.is_empty(), "{errs:?}");
    let g = vec!["T".to_string(), "U".to_string()];
    assert_eq!(derived(&m), vec![("Display".into(), "Box".into(), g.clone(), g)]);
}

#[test]
fn one_derive_may_name_several_protocols() {
    // The arg is an opaque string, so a list is the processor's own parsing rather than
    // new syntax -- the same division `@cfg` makes.
    assert_eq!(crate::derive::protocols(" Display , Display "), vec!["Display", "Display"]);
}

/// A `@cfg`-omitted record derives nothing, because it is gone before the derive pass runs.
#[test]
fn a_dropped_record_derives_nothing() {
    let (m, _, errs) = run(
        r#"@cfg("windows") @derive("Display") record P { x: i64 }"#,
        Config::default(),
    );
    assert!(errs.is_empty(), "{errs:?}");
    assert!(derived(&m).is_empty(), "{:?}", derived(&m));
}

#[test]
fn derive_is_only_for_a_record() {
    let (_, _, errs) = run(r#"@derive("Display") fn f() {}"#, Config::default());
    assert!(errs.iter().any(|e| e.message.contains("not a fn")), "{errs:?}");
}

#[test]
fn derive_needs_an_argument() {
    let (_, _, errs) = run("@derive record P { x: i64 }", Config::default());
    assert!(errs.iter().any(|e| e.message.contains("needs the protocol")), "{errs:?}");
}

/// An underivable protocol is an error, not a silently missing impl -- which would surface
/// as "no impl" against a call site that looks perfectly correct.
#[test]
fn an_underivable_protocol_is_an_error() {
    let (m, _, errs) = run(r#"@derive("Serialize") record P { x: i64 }"#, Config::default());
    assert!(errs.iter().any(|e| e.message.contains("cannot derive `Serialize`")), "{errs:?}");
    assert!(derived(&m).is_empty());
}
