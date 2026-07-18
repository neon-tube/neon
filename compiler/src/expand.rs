//! The annotation-expansion pass: built-in processors that run over the parsed AST
//! between parsing and type-checking.
//!
//! An annotation is `@name` or `@name("arg")` on a `record`, `protocol`, `impl`, `fn`
//! or `mod`. Each name maps to exactly one built-in processor; an unrecognised name is
//! an error, not a silent no-op, so a typo'd `@cfg` cannot quietly miscompile. A
//! processor sees the node its annotation is on and decides whether the node survives
//! (`@cfg` drops code the target does not want) and may pull metadata off it into a
//! side table (`@doc`). The arg is an opaque string: a processor brings its own parser.
//!
//! This runs before the checker so a dropped branch is never type-checked and never
//! has to resolve.

use crate::ast::{self, Annotation, Decl, DeclKind, FnDecl, Module};
use crate::lexer::Span;
use std::collections::HashSet;

/// A diagnostic from expansion, rendered like any other: a span and a message.
#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    pub span: Span,
    pub message: String,
}

/// Metadata a processor pulls off the AST without changing its meaning — today just
/// the `@doc` text, keyed by the name of the thing it documents.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Meta {
    pub docs: Vec<(String, String)>,
}

/// The active `@cfg` keys. Empty by default; the driver fills it from the target and
/// `neon.toml`. A key is true iff it is in this set.
#[derive(Debug, Clone, Default)]
pub struct Config {
    keys: HashSet<String>,
}

impl Config {
    pub fn with(keys: impl IntoIterator<Item = String>) -> Self {
        Config { keys: keys.into_iter().collect() }
    }
}

/// The node an annotation sits on, borrowed for the processor to inspect. A method is a
/// `Fn`, so `@native` on a primitive impl's method and on a free fn are one case.
pub enum Target<'a> {
    Fn(&'a FnDecl),
    Record(&'a ast::RecordDecl),
    Protocol(&'a ast::ProtocolDecl),
    Impl(&'a ast::ImplDecl),
    Mod(&'a ast::ModDecl),
}

impl Target<'_> {
    fn what(&self) -> &'static str {
        match self {
            Target::Fn(_) => "fn",
            Target::Record(_) => "record",
            Target::Protocol(_) => "protocol",
            Target::Impl(_) => "impl",
            Target::Mod(_) => "mod",
        }
    }
}

/// What a processor decides about the node it ran on.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Decision {
    Keep,
    Omit,
}

struct Context<'a> {
    config: &'a Config,
    meta: &'a mut Meta,
    errors: &'a mut Vec<Error>,
}

impl Context<'_> {
    fn error(&mut self, span: Span, message: impl Into<String>) {
        self.errors.push(Error { span, message: message.into() });
    }
}

/// One built-in processor. Stateless: a processor is a rule, not an object.
trait Processor {
    fn run(&self, ann: &Annotation, target: &Target, cx: &mut Context) -> Decision;
}

/// The registry. A name that is not here is an unknown annotation.
fn lookup(name: &str) -> Option<&'static dyn Processor> {
    static NATIVE: Native = Native;
    static CFG: Cfg = Cfg;
    static DOC: Doc = Doc;
    match name {
        "native" => Some(&NATIVE),
        "cfg" => Some(&CFG),
        "doc" => Some(&DOC),
        _ => None,
    }
}

// ---- the pass ----

/// Expand a whole module. Returns the transformed module (with `@cfg`-omitted nodes
/// removed), the metadata gathered, and any diagnostics.
pub fn expand(module: Module, config: &Config) -> (Module, Meta, Vec<Error>) {
    let mut meta = Meta::default();
    let mut errors = Vec::new();
    let mut cx = Context { config, meta: &mut meta, errors: &mut errors };
    let decls = expand_decls(module.decls, &mut cx);
    (Module { decls }, meta, errors)
}

fn expand_decls(decls: Vec<Decl>, cx: &mut Context) -> Vec<Decl> {
    let mut out = Vec::new();
    for decl in decls {
        if let Some(decl) = expand_decl(decl, cx) {
            out.push(decl);
        }
    }
    out
}

fn expand_decl(decl: Decl, cx: &mut Context) -> Option<Decl> {
    // A decl's own annotations decide whether it survives.
    let decision = match &decl.kind {
        DeclKind::Fn(f) => run(&f.annotations, &Target::Fn(f), cx),
        DeclKind::Record(r) => run(&r.annotations, &Target::Record(r), cx),
        DeclKind::Protocol(p) => run(&p.annotations, &Target::Protocol(p), cx),
        DeclKind::Impl(i) => run(&i.annotations, &Target::Impl(i), cx),
        DeclKind::Mod(m) => run(&m.annotations, &Target::Mod(m), cx),
        _ => Decision::Keep,
    };
    if decision == Decision::Omit {
        return None;
    }
    // Then its children: a mod's decls, and the methods of a protocol or impl, each of
    // which may carry `@native` or its own `@cfg`.
    Some(match decl.kind {
        DeclKind::Mod(mut m) => {
            m.decls = expand_decls(m.decls, cx);
            Decl { kind: DeclKind::Mod(m), ..decl }
        }
        DeclKind::Protocol(mut p) => {
            p.methods = expand_methods(p.methods, cx);
            Decl { kind: DeclKind::Protocol(p), ..decl }
        }
        DeclKind::Impl(mut i) => {
            i.methods = expand_methods(i.methods, cx);
            Decl { kind: DeclKind::Impl(i), ..decl }
        }
        _ => decl,
    })
}

fn expand_methods(methods: Vec<FnDecl>, cx: &mut Context) -> Vec<FnDecl> {
    methods
        .into_iter()
        .filter(|m| run(&m.annotations, &Target::Fn(m), cx) == Decision::Keep)
        .collect()
}

/// Run every annotation on a node. An unknown name is an error. `Omit` wins: if any
/// annotation drops the node, it is dropped.
fn run(anns: &[Annotation], target: &Target, cx: &mut Context) -> Decision {
    let mut decision = Decision::Keep;
    for ann in anns {
        match lookup(&ann.name) {
            Some(p) => {
                if p.run(ann, target, cx) == Decision::Omit {
                    decision = Decision::Omit;
                }
            }
            None => cx.error(ann.span.clone(), format!("unknown annotation `@{}`", ann.name)),
        }
    }
    decision
}

// ---- the built-in processors ----

/// `@native("symbol")` — the fn's body is a runtime symbol. It requires the symbol and
/// a body-less fn; it never changes the AST, it is a marker codegen reads later.
struct Native;
impl Processor for Native {
    fn run(&self, ann: &Annotation, target: &Target, cx: &mut Context) -> Decision {
        match target {
            Target::Fn(f) => {
                if ann.arg.is_none() {
                    cx.error(ann.span.clone(), "`@native` needs the runtime symbol, e.g. `@native(\"neon_str_len\")`");
                }
                if f.body.is_some() {
                    cx.error(ann.span.clone(), "`@native` fn must have no body: its body is the runtime symbol");
                }
            }
            other => cx.error(
                ann.span.clone(),
                format!("`@native` is only for a `fn`, not a `{}`", other.what()),
            ),
        }
        Decision::Keep
    }
}

/// `@doc("text")` — pull the text into the metadata table, keep the node. Any target.
struct Doc;
impl Processor for Doc {
    fn run(&self, ann: &Annotation, target: &Target, cx: &mut Context) -> Decision {
        match &ann.arg {
            Some(text) => cx.meta.docs.push((target_name(target), text.clone())),
            None => cx.error(ann.span.clone(), "`@doc` needs its text, e.g. `@doc(\"what this is\")`"),
        }
        Decision::Keep
    }
}

/// `@cfg("cond")` — keep the node iff `cond` holds against the active config. `cond` is
/// `key`, `not(cond)`, `all(cond, ..)` or `any(cond, ..)`; `@cfg` brings its own parser.
struct Cfg;
impl Processor for Cfg {
    fn run(&self, ann: &Annotation, _target: &Target, cx: &mut Context) -> Decision {
        let Some(src) = &ann.arg else {
            cx.error(ann.span.clone(), "`@cfg` needs a condition, e.g. `@cfg(\"linux\")`");
            return Decision::Keep;
        };
        match eval_cfg(src, cx.config) {
            Ok(true) => Decision::Keep,
            Ok(false) => Decision::Omit,
            Err(msg) => {
                cx.error(ann.span.clone(), format!("`@cfg`: {msg}"));
                Decision::Keep
            }
        }
    }
}

fn target_name(target: &Target) -> String {
    match target {
        Target::Fn(f) => f.name.clone(),
        Target::Record(r) => r.name.clone(),
        Target::Protocol(p) => p.name.clone(),
        Target::Impl(i) => format!("{} for {:?}", i.protocol.join("::"), i.target.kind),
        Target::Mod(m) => m.name.clone(),
    }
}

// ---- the `@cfg` mini-language ----

/// `key | not(cond) | all(cond, ..) | any(cond, ..)`, evaluated against `config`. A
/// tiny recursive-descent parser over the raw string, so `@cfg` owns its own grammar.
fn eval_cfg(src: &str, config: &Config) -> Result<bool, String> {
    let tokens = cfg_tokens(src)?;
    let mut p = CfgParser { tokens: &tokens, pos: 0 };
    let v = p.cond(config)?;
    if p.pos != p.tokens.len() {
        return Err(format!("unexpected `{}` after the condition", p.tokens[p.pos]));
    }
    Ok(v)
}

fn cfg_tokens(src: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut word = String::new();
    for c in src.chars() {
        match c {
            '(' | ')' | ',' => {
                if !word.is_empty() {
                    out.push(std::mem::take(&mut word));
                }
                out.push(c.to_string());
            }
            c if c.is_whitespace() => {
                if !word.is_empty() {
                    out.push(std::mem::take(&mut word));
                }
            }
            c if c.is_alphanumeric() || c == '_' || c == '-' => word.push(c),
            other => return Err(format!("unexpected character `{other}` in the condition")),
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    Ok(out)
}

struct CfgParser<'a> {
    tokens: &'a [String],
    pos: usize,
}

impl CfgParser<'_> {
    fn peek(&self) -> Option<&str> {
        self.tokens.get(self.pos).map(String::as_str)
    }
    fn bump(&mut self) -> Option<&str> {
        let t = self.tokens.get(self.pos).map(String::as_str);
        if t.is_some() {
            self.pos += 1;
        }
        t
    }
    fn expect(&mut self, tok: &str) -> Result<(), String> {
        match self.bump() {
            Some(t) if t == tok => Ok(()),
            Some(t) => Err(format!("expected `{tok}`, found `{t}`")),
            None => Err(format!("expected `{tok}`, found end of condition")),
        }
    }

    fn cond(&mut self, config: &Config) -> Result<bool, String> {
        let head = self.bump().ok_or("empty condition")?.to_string();
        match head.as_str() {
            "not" => {
                self.expect("(")?;
                let v = self.cond(config)?;
                self.expect(")")?;
                Ok(!v)
            }
            "all" | "any" => {
                self.expect("(")?;
                let all = head == "all";
                let mut acc = all;
                loop {
                    let v = self.cond(config)?;
                    acc = if all { acc && v } else { acc || v };
                    match self.peek() {
                        Some(",") => {
                            self.bump();
                        }
                        _ => break,
                    }
                }
                self.expect(")")?;
                Ok(acc)
            }
            "(" | ")" | "," => Err(format!("expected a condition, found `{head}`")),
            key => Ok(config.keys.contains(key)),
        }
    }
}

#[cfg(test)]
mod tests;
