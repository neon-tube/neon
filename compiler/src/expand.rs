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
    /// `@runtime("neon_file")` — record name to the C type the backend must use for it.
    /// This is what lets a runtime-backed type be declared in a stdlib module instead of
    /// being a name the compiler recognises.
    pub runtime: Vec<(String, String)>,
}

/// The active `@cfg` keys. Empty by default; the driver fills it from the target and
/// `neon.toml`. A key is true iff it is in this set.
#[derive(Debug, Clone, Default)]
pub struct Config {
    keys: HashSet<String>,
}

impl Config {
    /// There is no way to *unset* a key: the set is built once by the driver and read-only
    /// thereafter, so every `@cfg` in a compilation sees the same answer for a key. A
    /// condition that flipped mid-pass would drop one of two mutually exclusive branches
    /// and keep neither.
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
    /// The keyword, for the "`@x` is only for a `fn`, not a `record`" diagnostics. Every
    /// processor that restricts its target phrases the error the same way through this, so
    /// a new target kind cannot be described inconsistently in five places.
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

/// Everything a processor may touch. It gets the config to read, the metadata to add to,
/// and the error list — but not the AST, which is why a processor can only decide `Keep`
/// or `Omit` and never rewrite a node. That restriction is what keeps expansion from
/// becoming a macro system.
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
    static RUNTIME: Runtime = Runtime;
    static PURE: Pure = Pure;
    static INLINE: Inline = Inline;
    static DERIVE: Derive = Derive;
    match name {
        "native" => Some(&NATIVE),
        "cfg" => Some(&CFG),
        "derive" => Some(&DERIVE),
        "doc" => Some(&DOC),
        "runtime" => Some(&RUNTIME),
        "pure" => Some(&PURE),
        "inline" => Some(&INLINE),
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
    let mut module = Module { decls };

    // Derives are generated HERE, at the end of this function, and not by each driver in
    // turn. There are four pipelines that parse a module -- `frontend::check`,
    // `cmd/check.rs`, and the corpus harnesses -- and the only thing they all agree on is
    // calling `expand`. Wiring the pass into two of them produced a compiler where `neon
    // check` and `neon run` disagreed about the same file, and a corpus where every derived
    // impl was missing. A pass that has to be remembered in four places is a pass that will
    // be forgotten in one.
    //
    // After the processor walk, so a `@cfg`-omitted record derives nothing: it is gone from
    // `decls` by now and there is nothing left to walk.
    //
    // This does NOT hand the AST to a `Processor`. The restriction that keeps expansion
    // from becoming a macro system is on what a *processor* may do -- decide `Keep` or
    // `Omit`, never rewrite -- and that is intact. `@derive`'s processor above only
    // validates; the generation is an ordinary function over the module, which this happens
    // to be the right place to call.
    crate::derive::derive(&mut module);

    (module, meta, errors)
}

/// Expand a declaration list, dropping the ones no processor kept. Declarations are taken
/// by value throughout this pass rather than mutated in place, because omission removes a
/// node rather than blanking it — a `@cfg`-ed-out decl must leave no trace for name
/// resolution to find.
fn expand_decls(decls: Vec<Decl>, cx: &mut Context) -> Vec<Decl> {
    let mut out = Vec::new();
    for decl in decls {
        if let Some(decl) = expand_decl(decl, cx) {
            out.push(decl);
        }
    }
    out
}

/// One declaration, outside-in: a node's own annotations decide whether it survives
/// *before* its children are visited.
///
/// The order is the point. A `@cfg`-omitted `mod` returns here without its body ever being
/// walked, so annotations inside code the target does not want are never looked up and
/// never reported as unknown — which is what lets a module written for another platform
/// use annotations this build does not have. It also means metadata is not gathered from
/// omitted code: a `@doc` inside a dropped branch does not reach the table.
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

/// A protocol's or impl's methods. Filtering only — a method has no annotated children, so
/// there is nothing below it to descend into, and a fn *body* is never expanded: `@cfg` is
/// a declaration-level tool, not a statement-level one.
///
/// A `@cfg`-omitted method simply disappears from its impl, leaving the impl incomplete
/// for this target — expansion does not check that against the protocol, so whatever
/// diagnostic follows comes from the checker seeing a method that was never declared.
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

/// `@runtime("neon_file")` — the record is a pointer to a C type the runtime owns, not a
/// struct laid out from its fields. It is the declaration form of what used to be a name
/// the compiler recognised (`List`, `Map`, `File` were matched by string in `record_repr`),
/// so a runtime-backed type can live in an ordinary stdlib module.
///
/// The record must declare no fields: its contents are the runtime's business, and a field
/// would claim a layout that the C type, not the compiler, decides. Generic parameters are
/// fine and are carried through as the repr's arguments — that is how a payload's type
/// reaches the backend so a witness can be emitted for it.
struct Runtime;
impl Processor for Runtime {
    fn run(&self, ann: &Annotation, target: &Target, cx: &mut Context) -> Decision {
        match target {
            Target::Record(r) => {
                match &ann.arg {
                    Some(sym) => cx.meta.runtime.push((r.name.clone(), sym.clone())),
                    None => cx.error(
                        ann.span.clone(),
                        "`@runtime` needs the C type, e.g. `@runtime(\"neon_file\")`",
                    ),
                }
                if !r.fields.is_empty() {
                    cx.error(
                        ann.span.clone(),
                        "`@runtime` record must declare no fields: the runtime owns its \
                         layout, and a field here would describe one the C type does not have",
                    );
                }
            }
            other => cx.error(
                ann.span.clone(),
                format!("`@runtime` is only for a `record`, not a `{}`", other.what()),
            ),
        }
        Decision::Keep
    }
}

/// `@pure` — this native has no effect beyond its return value, so a call whose result is
/// unused may be deleted.
///
/// Only meaningful on a native: a Neon body's purity is *inferred* from its instructions.
/// It takes no argument, and the absence of it means effectful — the safe direction, since
/// forgetting it costs an optimisation while wrongly claiming it deletes real work. The
/// analysis this replaces guessed from the symbol's spelling and defaulted to pure, which
/// silently removed a resource construction along with the cleanup it existed to schedule.
struct Pure;
impl Processor for Pure {
    fn run(&self, ann: &Annotation, target: &Target, cx: &mut Context) -> Decision {
        match target {
            Target::Fn(f) => {
                if ann.arg.is_some() {
                    cx.error(ann.span.clone(), "`@pure` takes no argument");
                }
                if !f.annotations.iter().any(|a| a.name == "native") {
                    cx.error(
                        ann.span.clone(),
                        "`@pure` is only for an `@native` fn: a Neon body's purity is \
                         inferred from what it does",
                    );
                }
            }
            other => cx.error(
                ann.span.clone(),
                format!("`@pure` is only for a `fn`, not a `{}`", other.what()),
            ),
        }
        Decision::Keep
    }
}

/// `@inline` — emit the function so the C compiler must inline it at every call site.
///
/// A hint would do nothing: measured on the brainfuck benchmark, `__builtin_expect` on the
/// cold branch, outlining the cold path, and `static` linkage each moved it under 4%, and
/// only forcing the inline moved it at all (23%). So this is `always_inline`, not `inline`.
///
/// It is for a *small wrapper around a primitive* -- `list::set` is a bounds check and a
/// call -- where the call and its tagged-result return cost more than the body. Asking for
/// it on anything substantial trades size for nothing, which is why it is opt-in per
/// declaration rather than a heuristic.
///
/// Rejected on an `@native` fn: there is no body to inline, only a declaration of one that
/// lives in the runtime archive.
struct Inline;
impl Processor for Inline {
    fn run(&self, ann: &Annotation, target: &Target, cx: &mut Context) -> Decision {
        match target {
            Target::Fn(f) => {
                if ann.arg.is_some() {
                    cx.error(ann.span.clone(), "`@inline` takes no argument");
                }
                if f.annotations.iter().any(|a| a.name == "native") {
                    cx.error(
                        ann.span.clone(),
                        "`@inline` is not for an `@native` fn: its body is in the runtime \
                         archive, and there is nothing here to inline",
                    );
                }
            }
            other => cx.error(
                ann.span.clone(),
                format!("`@inline` is only for a `fn`, not a `{}`", other.what()),
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

/// `@derive("Display")` — the record gets an impl the author could have written.
///
/// This processor only VALIDATES. The impl is written by `crate::derive`, a pass that runs
/// after this one, because generating a declaration means holding the AST and `Context`
/// deliberately does not — that restriction is what keeps expansion from becoming a macro
/// system, and `@derive` is not the reason to give it up. The cost is that a legal
/// `@derive` is checked here and honoured there; the two must agree on `can_derive`, which
/// is why the list lives in one place and this asks it rather than repeating it.
///
/// Everything wrong with a `@derive` is an error here rather than a silent no-op, for the
/// same reason a typo'd `@cfg` is: the failure would otherwise be a missing impl reported
/// somewhere else entirely, against a call site that looks correct.
struct Derive;
impl Processor for Derive {
    fn run(&self, ann: &Annotation, target: &Target, cx: &mut Context) -> Decision {
        if !matches!(target, Target::Record(_)) {
            let what = target.what();
            cx.error(
                ann.span.clone(),
                format!("`@derive` is for a record, not a {what}: there are no fields to walk"),
            );
            return Decision::Keep;
        }
        match &ann.arg {
            None => cx.error(
                ann.span.clone(),
                "`@derive` needs the protocol to derive, e.g. `@derive(\"Display\")`",
            ),
            Some(arg) => {
                let names = crate::derive::protocols(arg);
                if names.is_empty() {
                    cx.error(
                        ann.span.clone(),
                        "`@derive` names no protocol; write `@derive(\"Display\")`",
                    );
                }
                for name in names {
                    if !crate::derive::can_derive(&name) {
                        cx.error(
                            ann.span.clone(),
                            format!(
                                "cannot derive `{name}`: the compiler can write `Display`. \
                                 Anything else is an ordinary `impl {name} for ..`"
                            ),
                        );
                    }
                }
            }
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

/// The key `@doc` files its text under.
///
/// Not a resolved path — a bare declared name, so two same-named items in different modules
/// collide in the table. That is tolerable only because `Meta::docs` is a `Vec` of pairs
/// that nothing looks up by key yet; a doc tool that wants to index by name will need the
/// module path threaded through here.
///
/// An `impl` has no name of its own, so one is synthesised from the protocol path and the
/// target's `Debug` formatting. The result is a Rust-shaped string, not Neon source, and is
/// not fit to show a user.
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

/// Split a condition into words and punctuation. There are no operators, so a word is a
/// run of alphanumerics, `_` and `-`; `-` is allowed because config keys are target-shaped
/// (`x86_64-unknown-linux`) and splitting one on the hyphen would turn a key into a
/// syntax error.
///
/// Anything else is rejected here rather than passed through as an unknown key, so a
/// mistyped `@cfg("linux && macos")` is a diagnostic instead of a condition that quietly
/// never holds.
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

    /// One condition. `all`/`any` fold over a comma-separated list, seeded with their
    /// identity (`true` for `all`, `false` for `any`), and both require at least one
    /// operand — the loop parses before it checks for a comma, so `all()` is an error
    /// rather than a vacuous `true`.
    ///
    /// A bare word is a key lookup and is *always* well-formed: an unrecognised key is
    /// false, not a diagnostic. There is no registry of legal keys to check against, and
    /// asking about a key this build has never heard of is the normal case.
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
