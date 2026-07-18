//! `neon fmt`: the AST, printed back.
//!
//! Three properties hold, and are tested:
//!
//! - **Round-trip.** `parse(format(parse(src)))` equals `parse(src)` with spans
//!   ignored. Formatting never changes what a program means. Parentheses are
//!   decided from `crate::ops` — the same table the parser builds its levels out
//!   of — rather than from a second ladder kept here. A formatter with its own
//!   ladder prints `1 - (2 - 3)` as `1 - 2 - 3`.
//! - **Idempotence.** `format(format(src)) == format(src)`.
//! - **Comments survive.** Every comment in the input appears in the output. A
//!   formatter that drops comments is worse than no formatter.
//!
//! Line breaks are the author's: the formatter normalizes spelling, spacing,
//! punctuation and indentation, and decides whether a construct is written on
//! one line or many by looking at how the author wrote it — see `is_broken`.

#[cfg(test)]
mod tests;

use crate::ast::*;
use crate::lexer::{self, LexError, Lexed, Span, Token, Trivia, TriviaKind};
use crate::ops;
use crate::parser::{self, ParseError};
use std::fmt::Write as _;

/// Why a source could not be formatted. Formatting a file the compiler cannot
/// read is not something to guess at.
#[derive(Debug)]
pub enum FormatError {
    Lex(Vec<LexError>),
    Parse(Vec<ParseError>),
}

/// Format a source file.
pub fn format(src: &str) -> Result<String, FormatError> {
    let lexed = lexer::lex_full(src).map_err(FormatError::Lex)?;
    let (module, errors) = parser::parse(&lexed.tokens, src.len());
    match module {
        Some(m) if errors.is_empty() => Ok(format_module(src, &lexed, &m)),
        _ => Err(FormatError::Parse(errors)),
    }
}

/// Format an already-parsed module. `lexed` and `module` must be `src`'s: the
/// printer reads spans back against it.
pub fn format_module(src: &str, lexed: &Lexed, module: &Module) -> String {
    let mut f = Fmt {
        src,
        lexed,
        out: String::new(),
        indent: 0,
        trivia: 0,
        last_end: 0,
        allow_gap: false,
        no_record_lit: false,
    };
    f.module(module);
    f.out
}

// ---- precedence classes above the binary ladder ----
//
// Derived from the table, so a new binary level cannot silently collide.

/// `-`, `!`, `bnot`.
const P_UNARY: u8 = ops::MAX_PREC + 1;

/// Call, index, field, `is`, `as`. Tighter than the prefix operators, so
/// `-x.f` is `-(x.f)`.
const P_POSTFIX: u8 = P_UNARY + 1;

const P_ATOM: u8 = P_POSTFIX + 1;

/// Forms that swallow the expression to their right: `return e`, `break e`,
/// `throw e`, `try e`, `(x) => e`. They sit where an atom sits but read to the
/// end of the expression, so `Binary { Add, Return(1), 2 }` can only be printed
/// as `(return 1) + 2`.
const P_GREEDY: u8 = 0;

/// Block-like forms: `if`, `match`, `loop`, `while`, `for`, `{ .. }`.
///
/// The grammar takes `loop { break 7; } + 1` and means `(loop { .. }) + 1`, so
/// printing it bare would round-trip. It is parenthesised anyway: unlike the
/// greedy forms above this is legibility rather than correctness, and nobody
/// should have to know the answer to that question.
const P_BLOCK_LIKE: u8 = P_GREEDY;

/// Type operators: `|` loosest, then `&`, then `!`.
const TP_UNION: u8 = 1;
const TP_INTERSECT: u8 = 2;
const TP_NEGATE: u8 = 3;
const TP_ATOM: u8 = 4;

/// A position that takes a whole type, and so needs no parentheses around
/// anything: an alias's value, a parameter, a return, a generic argument.
const TP_ANY: u8 = 0;

/// `(A) -> B | C` reads its return type to the end, like the greedy expression
/// forms, so a function type only fits where a whole type fits.
const TP_FN: u8 = TP_ANY;

const INDENT: &str = "    ";

struct Fmt<'a> {
    src: &'a str,
    lexed: &'a Lexed,
    out: String,
    indent: usize,
    /// Index of the next comment not yet printed. Everything before it is out;
    /// the final flush guarantees everything from it on gets out too.
    trivia: usize,
    /// Source offset of the end of the last thing printed, for blank-line
    /// recovery.
    last_end: usize,
    /// False at the start of a file and directly after a `{`, where a blank line
    /// is noise rather than the author's separation.
    allow_gap: bool,
    /// Set in condition position, where the grammar has no record literal
    /// because it would be ambiguous with the body that follows. Cleared inside
    /// any bracketed group, exactly as the parser returns to the full expression
    /// grammar there.
    no_record_lit: bool,
}

/// What `group_start` saved and `group_end` puts back.
struct Group {
    gap: bool,
    no_record_lit: bool,
}

/// A delimited group's shape, read off the source.
struct Layout {
    /// One item per line, with a trailing comma.
    broken: bool,
    /// Where the closing delimiter is, so a comment just before it lands inside
    /// the group rather than being carried out to the next item.
    close: usize,
}

impl<'a> Fmt<'a> {
    // ---- output primitives ----

    fn push(&mut self, s: &str) {
        self.out.push_str(s);
    }

    fn newline(&mut self) {
        self.out.push('\n');
    }

    fn write_indent(&mut self) {
        for _ in 0..self.indent {
            self.out.push_str(INDENT);
        }
    }

    fn advance(&mut self, end: usize) {
        self.last_end = self.last_end.max(end);
    }

    // ---- source queries ----

    /// Did the author write this across more than one line?
    fn is_broken(&self, span: &Span) -> bool {
        self.lexed.line_of(span.start) != self.lexed.line_of(span.end.saturating_sub(1))
    }

    fn has_trivia(&self, span: &Span) -> bool {
        self.lexed.trivia_within(span).next().is_some()
    }

    /// Is there a line break between the end of one thing and the start of the
    /// next?
    fn gap_broken(&self, a_end: usize, b_start: usize) -> bool {
        self.lexed.line_of(a_end.saturating_sub(1)) != self.lexed.line_of(b_start)
    }

    /// How a delimited group is laid out, and where its closing delimiter is.
    ///
    /// A group is broken iff the author put a line break at one of its
    /// *separator* positions: after the opening delimiter, between two items, or
    /// before the closing one.
    ///
    /// Neither cruder test works. `is_broken` over the group's own span says yes
    /// for `f(if c { .. } else { .. })`, where one item spans lines but the
    /// author never broke the group. `is_broken` over the items' extent misses
    /// the delimiters, and pulls
    ///
    /// ```text
    /// match s {
    ///     is Circle => "circle",
    /// }
    /// ```
    ///
    /// onto one line, because its single arm sits on a single line.
    fn layout(&self, from: usize, open: &Token, close: &Token, items: &[Span]) -> Layout {
        let open = self.token_span(from, open);
        let last_end = items.last().map_or(open.end, |s| s.end);
        let close = self.token_span(last_end, close);

        let mut broken = false;
        let mut prev_end = open.end;
        for item in items {
            broken |= self.gap_broken(prev_end, item.start);
            prev_end = item.end;
        }
        // An empty group has one gap, and no reason to open for it.
        broken = !items.is_empty() && (broken || self.gap_broken(prev_end, close.start));
        Layout { broken, close: close.start }
    }

    /// The span of the first `want` at or after `after`.
    fn token_span(&self, after: usize, want: &Token) -> Span {
        self.lexed
            .tokens
            .iter()
            .find(|t| t.span.start >= after && t.token == *want)
            .map_or(self.src.len()..self.src.len(), |t| t.span.clone())
    }

    /// The span of the last `want` ending at or before `before`. For a group
    /// whose opening delimiter sits before anything with a span of its own — a
    /// parameter list, whose `(` follows a name the AST stores as a bare String.
    fn token_span_before(&self, before: usize, want: &Token) -> Span {
        self.lexed
            .tokens
            .iter()
            .rev()
            .find(|t| t.span.end <= before && t.token == *want)
            .map_or(0..0, |t| t.span.clone())
    }

    /// The end of the group closed by the first `close` at or after `after`.
    /// Anything nested is already behind `after`, so the first one is ours.
    fn close_of(&self, after: usize, close: &Token) -> usize {
        self.token_span(after, close).end
    }

    /// The token at or after `offset`.
    fn token_at(&self, offset: usize) -> Option<&'a Token> {
        let i = self.lexed.tokens.partition_point(|t| t.span.start < offset);
        self.lexed.tokens.get(i).map(|t| &t.token)
    }

    /// Where the token at or after `offset` begins.
    fn token_start(&self, offset: usize) -> usize {
        let i = self.lexed.tokens.partition_point(|t| t.span.start < offset);
        self.lexed
            .tokens
            .get(i)
            .map_or(self.src.len(), |t| t.span.start)
    }

    /// Where the next function declaration at or after `offset` begins.
    ///
    /// `FnDecl` carries no span, and a protocol's or impl's methods are a bare
    /// `Vec<FnDecl>`, so their leading comments have to be found in the token
    /// stream. A method starts with an annotation or with `fn`, and neither
    /// token can occur in the signature that precedes it.
    fn fn_start(&self, offset: usize) -> usize {
        self.lexed
            .tokens
            .iter()
            .find(|t| t.span.start >= offset && matches!(t.token, Token::At | Token::Fn))
            .map_or(self.src.len(), |t| t.span.start)
    }

    /// Source text, verbatim. For the `Error` nodes, which exist only when the
    /// parse failed and `format` has already refused.
    fn verbatim(&mut self, span: &Span) {
        let src = self.src;
        self.push(src.get(span.start..span.end).unwrap_or_default());
        self.advance(span.end);
    }

    // ---- comments and blank lines ----

    /// A blank line here if the author left one. Runs of two or more collapse to
    /// one; none are deleted — a formatter that reflows every blank line on
    /// first run is not one anyone will use.
    fn gap(&mut self, start: usize) {
        if !self.allow_gap {
            self.allow_gap = true;
            return;
        }
        if self.lexed.blank_lines_between(self.last_end, start) > 0 {
            self.newline();
        }
    }

    /// Every comment ending before `before`, each on its own line at the current
    /// indent. Only call at the start of a line.
    fn flush(&mut self, before: usize) {
        let lexed = self.lexed;
        while let Some(t) = lexed.trivia.get(self.trivia) {
            if t.span.end > before {
                break;
            }
            self.gap(t.span.start);
            self.write_indent();
            self.comment(t);
            self.newline();
            self.last_end = t.span.end;
            self.trivia += 1;
        }
    }

    /// Comments before an item, then the author's blank line.
    fn leading(&mut self, start: usize) {
        self.flush(start);
        self.gap(start);
    }

    /// Comments on the same line as the end of what was just printed. Leaves the
    /// line open; the caller ends it.
    fn trailing(&mut self, end: usize) {
        let lexed = self.lexed;
        let line = lexed.line_of(end.saturating_sub(1));
        while let Some(t) = lexed.trivia.get(self.trivia) {
            if lexed.line_of(t.span.start) != line {
                break;
            }
            self.push("  ");
            self.comment(t);
            self.last_end = t.span.end;
            self.trivia += 1;
        }
    }

    /// The comment, delimiters back on and text untouched: a formatter that
    /// reflows prose inside a comment is one people turn off.
    fn comment(&mut self, t: &Trivia) {
        match t.kind {
            TriviaKind::Line => {
                self.push("//");
                self.push(t.text.trim_end());
            }
            TriviaKind::Doc => {
                self.push("///");
                self.push(t.text.trim_end());
            }
            TriviaKind::Block => {
                self.push("/*");
                self.push(&t.text);
                self.push("*/");
            }
        }
    }

    // ---- item groups ----

    /// Open the inside of a `[ ]`, `( )` or `{ }` group. The delimiters
    /// themselves are the caller's.
    fn group_start(&mut self, broken: bool) -> Group {
        let g = Group {
            gap: self.allow_gap,
            no_record_lit: self.no_record_lit,
        };
        // Inside brackets the parser is back on the full expression grammar,
        // record literals included.
        self.no_record_lit = false;
        if broken {
            self.newline();
            self.indent += 1;
            self.allow_gap = false;
        }
        g
    }

    /// Close it, leaving the cursor where the closing delimiter goes.
    /// `close_at` is that delimiter's offset, so a comment sitting just before
    /// it lands inside the group rather than being carried out.
    fn group_end(&mut self, broken: bool, close_at: Option<usize>, g: Group) {
        if broken {
            if let Some(close) = close_at {
                self.flush(close);
            }
            self.indent -= 1;
            self.write_indent();
        }
        self.allow_gap = g.gap;
        self.no_record_lit = g.no_record_lit;
    }

    /// The items of a group. Returns whether anything was written, so a caller
    /// with a tail item (`..rest`) knows whether it needs a separator.
    fn items<T>(
        &mut self,
        list: &[T],
        broken: bool,
        span_of: impl Fn(&T) -> Span,
        mut print: impl FnMut(&mut Self, &T),
    ) -> bool {
        for (i, item) in list.iter().enumerate() {
            let span = span_of(item);
            if broken {
                self.leading(span.start);
                self.write_indent();
                print(self, item);
                self.push(",");
                self.advance(span.end);
                self.trailing(self.last_end);
                self.newline();
            } else {
                if i > 0 {
                    self.push(", ");
                }
                print(self, item);
                self.advance(span.end);
            }
        }
        !list.is_empty()
    }

    /// A whole comma-separated group: `group_start`, `items`, `group_end`.
    fn seq<T>(
        &mut self,
        list: &[T],
        broken: bool,
        close_at: Option<usize>,
        span_of: impl Fn(&T) -> Span,
        print: impl FnMut(&mut Self, &T),
    ) {
        let g = self.group_start(broken);
        self.items(list, broken, span_of, print);
        self.group_end(broken, close_at, g);
    }

    /// A bracketed group with no line-breaking decision to make.
    fn grouped(&mut self, f: impl FnOnce(&mut Self)) {
        let saved = std::mem::replace(&mut self.no_record_lit, false);
        f(self);
        self.no_record_lit = saved;
    }

    // ---- module and declarations ----

    fn module(&mut self, m: &Module) {
        self.decls(&m.decls);
        // Whatever is left: a comment at the end of the file, and anything the
        // per-item flushes did not reach. Nothing is dropped.
        self.flush(usize::MAX);
    }

    fn decls(&mut self, decls: &[Decl]) {
        for d in decls {
            self.leading(d.span.start);
            self.write_indent();
            self.decl(d);
            self.advance(d.span.end);
            self.trailing(self.last_end);
            self.newline();
        }
    }

    fn decl(&mut self, d: &Decl) {
        match &d.kind {
            DeclKind::Fn(f) => self.fn_decl(f),
            DeclKind::Record(r) => self.record_decl(r, &d.span),
            DeclKind::Protocol(p) => self.protocol_decl(p, &d.span),
            DeclKind::Impl(i) => self.impl_decl(i, &d.span),
            // A type alias takes no trailing semicolon.
            DeclKind::TypeAlias(a) => {
                self.push("type ");
                self.alias(a);
            }
            DeclKind::MuType(a) => {
                self.push("mu type ");
                self.alias(a);
            }
            DeclKind::Newtype(a) => {
                self.push("newtype ");
                self.alias(a);
            }
            DeclKind::Use(u) => {
                self.push("use ");
                self.use_tree(&u.tree);
                self.push(";");
            }
            DeclKind::Mod(m) => {
                self.annotations(&m.annotations);
                if m.internal {
                    self.push("internal ");
                }
                self.push("mod ");
                self.push(&m.name);
                self.push(" ");
                let span = d.span.clone();
                self.decl_body(&span, |f| f.decls(&m.decls));
            }
            DeclKind::Const(c) => {
                self.push("const ");
                self.push(&c.name);
                if let Some(t) = &c.ty {
                    self.push(": ");
                    self.ty(t, TP_ANY);
                }
                self.push(" = ");
                self.expr(&c.value, P_GREEDY);
                self.push(";");
            }
            DeclKind::TestBlock(t) => {
                self.push(match t.kind {
                    TestKind::Test => "test ",
                    TestKind::Bench => "bench ",
                });
                self.str_verbatim(d.span.start);
                self.push(" ");
                self.block(&t.body);
            }
            DeclKind::Error => self.verbatim(&d.span),
        }
    }

    /// A `{ ... }` holding declarations. Always broken: nobody wants a module on
    /// one line.
    fn decl_body(&mut self, span: &Span, f: impl FnOnce(&mut Self)) {
        self.push("{");
        let g = self.group_start(true);
        f(self);
        self.group_end(true, Some(span.end.saturating_sub(1)), g);
        self.push("}");
        self.advance(span.end);
    }

    fn alias(&mut self, a: &AliasDecl) {
        self.push(&a.name);
        self.generic_params(&a.generics);
        self.push(" = ");
        self.ty(&a.value, TP_ANY);
    }

    fn annotations(&mut self, anns: &[Annotation]) {
        for a in anns {
            self.push("@");
            self.push(&a.name);
            if a.arg.is_some() {
                self.push("(");
                self.str_verbatim(a.span.start);
                self.push(")");
            }
            self.advance(a.span.end);
            // `@native("...") fn len(..)` on one line, or stacked above the
            // declaration: whichever the author chose.
            let next = self.token_start(a.span.end);
            if self.lexed.line_of(a.span.end) == self.lexed.line_of(next) {
                self.push(" ");
            } else {
                self.newline();
                self.write_indent();
            }
        }
    }

    fn generic_params(&mut self, generics: &[String]) {
        if generics.is_empty() {
            return;
        }
        self.push("[");
        self.push(&generics.join(", "));
        self.push("]");
    }

    fn fn_decl(&mut self, f: &FnDecl) {
        self.annotations(&f.annotations);
        self.push("fn ");
        self.push(&f.name);
        self.generic_params(&f.generics);

        // A function signature has no span of its own: its parameter list is
        // found from the `(` before the first parameter.
        let spans: Vec<Span> = f.params.iter().map(|p| p.span.clone()).collect();
        let l = match f.params.first() {
            Some(first) => {
                let open = self.token_span_before(first.span.start, &Token::LParen);
                self.layout(open.start, &Token::LParen, &Token::RParen, &spans)
            }
            None => Layout { broken: false, close: 0 },
        };
        self.push("(");
        self.seq(
            &f.params,
            l.broken,
            Some(l.close),
            |p| p.span.clone(),
            |s, p| {
                s.push(&p.name);
                s.push(": ");
                s.ty(&p.ty, TP_ANY);
            },
        );
        self.push(")");

        // `throws E` comes before `->`, and a bare arrow would rebind to that `->`
        // rather than stay the thrown type. Same rule as the arrow-type printer.
        if let Some(t) = &f.throws {
            self.push(" throws ");
            self.ty(t, TP_UNION);
        }
        if let Some(t) = &f.ret {
            self.push(" -> ");
            self.ty(t, TP_ANY);
        }
        if !f.wheres.is_empty() {
            self.push(" where ");
            for (i, w) in f.wheres.iter().enumerate() {
                if i > 0 {
                    self.push(", ");
                }
                self.push(&w.param);
                self.push(": ");
                self.ty(&w.bound, TP_ANY);
            }
        }
        // No body: a protocol's required method is its signature and nothing
        // else.
        if let Some(b) = &f.body {
            self.push(" ");
            self.block(b);
        }
    }

    fn record_decl(&mut self, r: &RecordDecl, span: &Span) {
        self.annotations(&r.annotations);
        if r.opaque {
            self.push("opaque ");
        }
        self.push("record ");
        self.push(&r.name);
        self.generic_params(&r.generics);
        self.push(" ");
        self.fields_braced(&r.fields, span);
    }

    /// `{ a: i64, b: str }`, `{}`, or one field per line.
    fn fields_braced(&mut self, fields: &[Field], span: &Span) {
        let stray = self.has_trivia(span);
        if fields.is_empty() && !stray {
            self.push("{}");
            return;
        }
        let spans: Vec<Span> = fields.iter().map(|f| f.span.clone()).collect();
        let l = self.layout(span.start, &Token::LBrace, &Token::RBrace, &spans);
        let broken = l.broken || (fields.is_empty() && stray);
        self.push("{");
        if !broken {
            self.push(" ");
        }
        self.seq(
            fields,
            broken,
            Some(l.close),
            |f| f.span.clone(),
            |s, f| {
                s.push(&f.name);
                s.push(": ");
                s.ty(&f.ty, TP_ANY);
            },
        );
        if !broken {
            self.push(" ");
        }
        self.push("}");
    }

    fn protocol_decl(&mut self, p: &ProtocolDecl, span: &Span) {
        self.annotations(&p.annotations);
        self.push("protocol ");
        self.push(&p.name);
        self.push(" for ");
        self.push(&p.subject);
        if p.subject_arity > 0 {
            self.push("[");
            self.push(&vec!["_"; p.subject_arity].join(", "));
            self.push("]");
        }
        if !p.wheres.is_empty() {
            self.push(" where ");
            for (i, w) in p.wheres.iter().enumerate() {
                if i > 0 {
                    self.push(", ");
                }
                self.push(&w.param);
                self.push(": ");
                self.ty(&w.bound, TP_ANY);
            }
        }
        self.push(" ");
        self.methods(&p.methods, span);
    }

    fn impl_decl(&mut self, i: &ImplDecl, span: &Span) {
        self.annotations(&i.annotations);
        if i.orphan {
            self.push("orphan ");
        }
        self.push("impl");
        self.generic_params(&i.generics);
        self.push(" ");
        self.path(&i.protocol);
        self.push(" for ");
        self.ty(&i.target, TP_ANY);
        self.push(" ");
        self.methods(&i.methods, span);
    }

    /// A protocol's or impl's methods.
    ///
    /// `FnDecl` has no span, so there is nothing to hand `layout`: the
    /// one-line-or-many decision falls back to whether the whole declaration is
    /// on one line, which is the same question for a body that holds nothing
    /// else.
    fn methods(&mut self, methods: &[FnDecl], span: &Span) {
        if methods.is_empty() && !self.has_trivia(span) {
            self.push("{}");
            return;
        }
        self.advance(span.start);
        let inline = !self.is_broken(span) && !self.has_trivia(span);
        self.push(if inline { "{ " } else { "{" });
        let g = self.group_start(!inline);
        for (i, m) in methods.iter().enumerate() {
            let start = self.fn_start(self.last_end);
            if inline {
                if i > 0 {
                    self.push(" ");
                }
                self.fn_decl(m);
            } else {
                self.leading(start);
                self.write_indent();
                self.fn_decl(m);
                self.trailing(self.last_end);
                self.newline();
            }
        }
        self.group_end(!inline, Some(span.end.saturating_sub(1)), g);
        self.push(if inline { " }" } else { "}" });
        self.advance(span.end);
    }

    // ---- types ----

    fn ty(&mut self, t: &TypeSpec, min_prec: u8) {
        if type_prec(t) < min_prec {
            self.push("(");
            self.ty_inner(t);
            self.push(")");
        } else {
            self.ty_inner(t);
        }
        self.advance(t.span.end);
    }

    fn ty_inner(&mut self, t: &TypeSpec) {
        match &t.kind {
            TypeSpecKind::Named { path, args } => {
                self.path(path);
                if !args.is_empty() {
                    self.push("[");
                    self.ty_run(args, ", ", TP_ANY);
                    self.push("]");
                }
            }
            TypeSpecKind::Atom(a) => {
                self.push(":");
                self.push(a);
            }
            TypeSpecKind::Null => self.push("null"),
            TypeSpecKind::Any => self.push("any"),
            TypeSpecKind::Struct(fields) => self.fields_braced(fields, &t.span),
            TypeSpecKind::Union(v) => self.ty_run(v, " | ", TP_INTERSECT),
            TypeSpecKind::Intersect(v) => self.ty_run(v, " & ", TP_NEGATE),
            TypeSpecKind::Negate(inner) => {
                self.push("!");
                self.ty(inner, TP_NEGATE);
            }
            TypeSpecKind::Fn { params, throws, ret } => {
                self.push("(");
                self.ty_run(params, ", ", TP_ANY);
                self.push(")");
                if let Some(t) = throws {
                    self.push(" throws ");
                    // A bare arrow here would rebind to this clause's own `->`.
                    self.ty(t, TP_UNION);
                }
                self.push(" -> ");
                self.ty(ret, TP_ANY);
            }
            TypeSpecKind::Tuple(v) => {
                self.push("(");
                self.ty_run(v, ", ", TP_ANY);
                self.push(")");
            }
            TypeSpecKind::Error => self.verbatim(&t.span),
        }
    }

    fn ty_run(&mut self, items: &[TypeSpec], sep: &str, min_prec: u8) {
        for (i, t) in items.iter().enumerate() {
            if i > 0 {
                self.push(sep);
            }
            self.ty(t, min_prec);
        }
    }

    // ---- statements ----

    fn block(&mut self, b: &Block) {
        let empty = b.stmts.is_empty() && b.tail.is_none();
        let stray = self.has_trivia(&b.span);
        if empty && !stray {
            self.push("{}");
            self.advance(b.span.end);
            return;
        }
        // A block the author kept on one line stays on one line.
        let inline = !self.is_broken(&b.span) && !stray;
        self.push(if inline { "{ " } else { "{" });
        let g = self.group_start(!inline);
        self.block_items(b, inline);
        self.group_end(!inline, Some(b.span.end.saturating_sub(1)), g);
        self.push(if inline { " }" } else { "}" });
        self.advance(b.span.end);
    }

    fn block_items(&mut self, b: &Block, inline: bool) {
        for (i, s) in b.stmts.iter().enumerate() {
            self.begin_item(inline, i > 0, &s.span);
            self.stmt(s);
            self.end_item(inline, s.span.end);
        }
        if let Some(t) = &b.tail {
            self.begin_item(inline, !b.stmts.is_empty(), &t.span);
            self.expr(t, P_GREEDY);
            self.end_item(inline, t.span.end);
        }
    }

    fn begin_item(&mut self, inline: bool, sep: bool, span: &Span) {
        if inline {
            if sep {
                self.push(" ");
            }
        } else {
            self.leading(span.start);
            self.write_indent();
        }
    }

    fn end_item(&mut self, inline: bool, end: usize) {
        self.advance(end);
        if !inline {
            self.trailing(self.last_end);
            self.newline();
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        match &s.kind {
            StmtKind::Let { pat, ty, value } => {
                self.push("let ");
                self.pattern(pat);
                if let Some(t) = ty {
                    self.push(": ");
                    self.ty(t, TP_ANY);
                }
                self.push(" = ");
                self.expr(value, P_GREEDY);
                self.push(";");
            }
            StmtKind::Assign { name, value } => {
                self.push(name);
                self.push(" = ");
                self.expr(value, P_GREEDY);
                self.push(";");
            }
            StmtKind::Expr(e) => {
                self.expr(e, P_GREEDY);
                // An expression ending in a block stands alone; anything else
                // needs its semicolon. Where one is optional the author's choice
                // is read back out of the token stream: dropping a `;` written
                // after `if c { .. } else { .. }` would let the next statement
                // be swallowed as a continuation of it.
                if !is_block_like(e) || self.token_at(e.span.end) == Some(&Token::Semi) {
                    self.push(";");
                }
            }
            StmtKind::Error => self.verbatim(&s.span),
        }
    }

    // ---- patterns ----

    fn pattern(&mut self, p: &Pattern) {
        match &p.kind {
            PatternKind::Wildcard => self.push("_"),
            PatternKind::Bind(n) => self.push(n),
            PatternKind::Is(t) => {
                self.push("is ");
                self.ty(t, TP_ANY);
            }
            PatternKind::Literal(e) => self.expr(e, P_ATOM),
            PatternKind::Record { path, fields, rest } => {
                if let Some(path) = path {
                    self.path(path);
                    self.push(" ");
                }
                if fields.is_empty() && !rest {
                    self.push("{}");
                } else {
                    let spans: Vec<Span> = fields.iter().map(|f| f.span.clone()).collect();
                    let l = self.layout(p.span.start, &Token::LBrace, &Token::RBrace, &spans);
                    let broken = l.broken;
                    self.push("{");
                    if !broken {
                        self.push(" ");
                    }
                    let g = self.group_start(broken);
                    let any = self.items(
                        fields,
                        broken,
                        |f| f.span.clone(),
                        |s, f| {
                            s.push(&f.name);
                            // `{ x }` binds the field to `x`.
                            if let Some(inner) = &f.pat {
                                s.push(": ");
                                s.pattern(inner);
                            }
                        },
                    );
                    // `..` closes the list; no comma may follow it.
                    if *rest {
                        if broken {
                            self.write_indent();
                        } else if any {
                            self.push(", ");
                        }
                        self.push("..");
                        if broken {
                            self.newline();
                        }
                    }
                    self.group_end(broken, Some(l.close), g);
                    if !broken {
                        self.push(" ");
                    }
                    self.push("}");
                }
            }
            PatternKind::Tuple(v) => {
                self.push("(");
                for (i, inner) in v.iter().enumerate() {
                    if i > 0 {
                        self.push(", ");
                    }
                    self.pattern(inner);
                }
                self.push(")");
            }
            PatternKind::Error => self.verbatim(&p.span),
        }
        self.advance(p.span.end);
    }

    // ---- expressions ----

    /// An expression, parenthesised when the position it sits in binds tighter
    /// than it does.
    fn expr(&mut self, e: &Expr, min_prec: u8) {
        let parens = expr_prec(e) < min_prec
            // In condition position the grammar has no record literal, so one
            // that reaches here has to be lifted back into the full grammar.
            || (self.no_record_lit && matches!(e.kind, ExprKind::RecordLit { .. }));
        if parens {
            self.push("(");
            self.grouped(|s| s.expr_inner(e));
            self.push(")");
        } else {
            self.expr_inner(e);
        }
        self.advance(e.span.end);
    }

    /// An expression in condition position: `if`, `while`, `for .. in` and a
    /// match scrutinee all sit directly before a `{`.
    fn cond(&mut self, e: &Expr) {
        let saved = std::mem::replace(&mut self.no_record_lit, true);
        self.expr(e, P_GREEDY);
        self.no_record_lit = saved;
    }

    fn expr_inner(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Int(n) => {
                if !self.lit_verbatim(&e.span, &Token::Int(*n)) {
                    let mut s = String::new();
                    let _ = write!(s, "{n}");
                    self.push(&s);
                }
            }
            // `1.0`, `1.00` and `1e0` are the same value and not the same text.
            ExprKind::Float(f) => {
                if !self.lit_verbatim(&e.span, &Token::Float(f.clone())) {
                    self.push(f);
                }
            }
            ExprKind::Str(parts) => self.str_parts(parts, &e.span),
            ExprKind::Rune(c) => {
                if !self.lit_verbatim(&e.span, &Token::Rune(*c)) {
                    let mut s = String::from("'");
                    escape_char(*c, '\'', &mut s);
                    s.push('\'');
                    self.push(&s);
                }
            }
            ExprKind::Atom(a) => {
                self.push(":");
                self.push(a);
            }
            ExprKind::Bool(b) => self.push(if *b { "true" } else { "false" }),
            ExprKind::Null => self.push("null"),
            ExprKind::Path(p) => self.path(p),
            ExprKind::Unary { op, rhs } => {
                self.push(match op {
                    UnOp::Neg => "-",
                    UnOp::Not => "!",
                    UnOp::Bnot => "bnot ",
                });
                // `-(-5)`, not `--5`: two prefix operators run together read as
                // one operator, and the one they look like does not exist here.
                if matches!(rhs.kind, ExprKind::Unary { .. }) {
                    self.push("(");
                    self.grouped(|s| s.expr(rhs, P_GREEDY));
                    self.push(")");
                } else {
                    self.expr(rhs, P_UNARY);
                }
            }
            ExprKind::Binary { op, lhs, rhs } => {
                // Left-associative, every one of them: the left operand may be
                // the same level, the right may not.
                self.expr(lhs, op.prec());
                self.push(" ");
                self.push(op.text());
                self.push(" ");
                self.expr(rhs, op.prec() + 1);
            }
            ExprKind::Call { callee, generics, args } => {
                self.expr(callee, P_POSTFIX);
                if !generics.is_empty() {
                    self.grouped(|s| {
                        s.push("[");
                        s.ty_run(generics, ", ", TP_ANY);
                        s.push("]");
                    });
                }
                let spans: Vec<Span> = args.iter().map(|a| a.span.clone()).collect();
                let l = self.layout(callee.span.end, &Token::LParen, &Token::RParen, &spans);
                self.push("(");
                self.seq(
                    args,
                    l.broken,
                    Some(l.close),
                    |a| a.span.clone(),
                    |s, a| s.expr(a, P_GREEDY),
                );
                self.push(")");
            }
            ExprKind::Index { base, index } => {
                self.expr(base, P_POSTFIX);
                self.push("[");
                self.grouped(|s| s.expr(index, P_GREEDY));
                self.push("]");
            }
            ExprKind::Field { base, name } => {
                self.expr(base, P_POSTFIX);
                self.push(".");
                self.push(name);
            }
            ExprKind::List(elems) => {
                let spans: Vec<Span> =
                    elems.iter().map(|el| elem_expr(el).span.clone()).collect();
                let l = self.layout(e.span.start, &Token::LBracket, &Token::RBracket, &spans);
                self.push("[");
                self.seq(
                    elems,
                    l.broken,
                    Some(l.close),
                    |el| elem_expr(el).span.clone(),
                    |s, el| match el {
                        Elem::Value(v) => s.expr(v, P_GREEDY),
                        Elem::Spread(v) => {
                            s.push("..");
                            s.expr(v, P_GREEDY);
                        }
                    },
                );
                self.push("]");
            }
            ExprKind::RecordLit { path, fields, spread } => {
                self.record_lit(path.as_deref(), fields, spread.as_deref(), &e.span)
            }
            ExprKind::Tuple(v) => {
                let spans: Vec<Span> = v.iter().map(|e| e.span.clone()).collect();
                let l = self.layout(e.span.start, &Token::LParen, &Token::RParen, &spans);
                self.push("(");
                self.seq(
                    v,
                    l.broken,
                    Some(l.close),
                    |e| e.span.clone(),
                    |s, e| s.expr(e, P_GREEDY),
                );
                self.push(")");
            }
            ExprKind::Lambda { params, body } => {
                self.grouped(|s| {
                    s.push("(");
                    for (i, p) in params.iter().enumerate() {
                        if i > 0 {
                            s.push(", ");
                        }
                        s.push(&p.name);
                        if let Some(t) = &p.ty {
                            s.push(": ");
                            s.ty(t, TP_ANY);
                        }
                    }
                    s.push(")");
                });
                self.push(" => ");
                let body_prec = if is_block_like(body) { P_BLOCK_LIKE } else { P_UNARY };
                self.expr(body, body_prec);
            }
            ExprKind::If { cond, then, else_ } => {
                self.push("if ");
                self.cond(cond);
                self.push(" ");
                self.block(then);
                if let Some(e) = else_ {
                    // Whether `else` hangs off the `}` or starts its own line is
                    // the author's, and no block's span records it.
                    let kw = self.token_span(then.span.end, &Token::Else);
                    if self.lexed.line_of(kw.start)
                        == self.lexed.line_of(then.span.end.saturating_sub(1))
                    {
                        self.push(" ");
                    } else {
                        self.newline();
                        self.write_indent();
                    }
                    self.push("else ");
                    // An `else if` chain stays flat rather than nesting.
                    match &e.kind {
                        ExprKind::Block(b) => self.block(b),
                        _ => self.expr(e, P_BLOCK_LIKE),
                    }
                    self.advance(e.span.end);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.push("match ");
                self.cond(scrutinee);
                self.push(" ");
                if arms.is_empty() && !self.has_trivia(&e.span) {
                    self.push("{}");
                    return;
                }
                let spans: Vec<Span> = arms.iter().map(|a| a.span.clone()).collect();
                let l = self.layout(scrutinee.span.end, &Token::LBrace, &Token::RBrace, &spans);
                let broken = l.broken;
                self.push("{");
                if !broken {
                    self.push(" ");
                }
                self.seq(
                    arms,
                    broken,
                    Some(l.close),
                    |a| a.span.clone(),
                    |s, a| {
                        s.pattern(&a.pat);
                        if let Some(g) = &a.guard {
                            s.push(" if ");
                            s.expr(g, P_GREEDY);
                        }
                        s.push(" => ");
                        s.expr(&a.body, P_GREEDY);
                    },
                );
                if !broken {
                    self.push(" ");
                }
                self.push("}");
            }
            ExprKind::Block(b) => self.block(b),
            ExprKind::Loop { body } => {
                self.push("loop ");
                self.block(body);
            }
            ExprKind::While { cond, body } => {
                self.push("while ");
                self.cond(cond);
                self.push(" ");
                self.block(body);
            }
            ExprKind::For { pat, iter, body } => {
                self.push("for ");
                self.pattern(pat);
                self.push(" in ");
                self.cond(iter);
                self.push(" ");
                self.block(body);
            }
            ExprKind::Break(v) => {
                self.push("break");
                if let Some(v) = v {
                    self.push(" ");
                    self.expr(v, P_GREEDY);
                }
            }
            ExprKind::Continue => self.push("continue"),
            ExprKind::Return(v) => {
                self.push("return");
                if let Some(v) = v {
                    self.push(" ");
                    self.expr(v, P_GREEDY);
                }
            }
            ExprKind::Throw(v) => {
                self.push("throw ");
                self.expr(v, P_GREEDY);
            }
            ExprKind::Try { form, body, catch } => {
                self.push(match form {
                    TryForm::Propagate => "try ",
                    TryForm::Soften => "try? ",
                    TryForm::Assert => "try! ",
                });
                let body_prec = if is_block_like(body) { P_BLOCK_LIKE } else { P_UNARY };
                self.expr(body, body_prec);
                if let Some(c) = catch {
                    self.push(" catch (");
                    self.push(&c.binding);
                    self.push(") ");
                    self.block(&c.body);
                    self.advance(c.span.end);
                }
            }
            ExprKind::Is { lhs, ty } => {
                self.expr(lhs, P_POSTFIX);
                self.push(" is ");
                self.ty(ty, TP_ANY);
            }
            ExprKind::As { lhs, ty } => {
                self.expr(lhs, P_POSTFIX);
                self.push(" as ");
                self.ty(ty, TP_ANY);
            }
            ExprKind::Assert { kind, args } => {
                self.push(match kind {
                    AssertKind::Assert => "assert",
                    AssertKind::Eq => "assert_eq",
                    AssertKind::Ne => "assert_ne",
                    AssertKind::Throws => "assert_throws",
                });
                let spans: Vec<Span> = args.iter().map(|a| a.span.clone()).collect();
                let l = self.layout(e.span.start, &Token::LParen, &Token::RParen, &spans);
                self.push("(");
                self.seq(
                    args,
                    l.broken,
                    Some(l.close),
                    |a| a.span.clone(),
                    |s, a| s.expr(a, P_GREEDY),
                );
                self.push(")");
            }
            ExprKind::Error => self.verbatim(&e.span),
        }
    }

    fn record_lit(
        &mut self,
        path: Option<&[String]>,
        fields: &[FieldInit],
        spread: Option<&Expr>,
        span: &Span,
    ) {
        if let Some(p) = path {
            self.path(p);
            self.push(" ");
        }
        if fields.is_empty() && spread.is_none() {
            self.push("{}");
            return;
        }
        let spans: Vec<Span> = fields
            .iter()
            .map(|f| f.span.clone())
            .chain(spread.map(|s| s.span.clone()))
            .collect();
        let l = self.layout(span.start, &Token::LBrace, &Token::RBrace, &spans);
        let broken = l.broken;
        self.push("{");
        if !broken {
            self.push(" ");
        }
        let g = self.group_start(broken);
        let any = self.items(
            fields,
            broken,
            |f| f.span.clone(),
            |s, f| {
                s.push(&f.name);
                s.push(": ");
                s.expr(&f.value, P_GREEDY);
            },
        );
        // The spread closes the literal: `Point { x: 1, ..base }`, and no comma
        // may follow it.
        if let Some(sp) = spread {
            if broken {
                self.leading(sp.span.start);
                self.write_indent();
            } else if any {
                self.push(", ");
            }
            self.push("..");
            self.expr(sp, P_GREEDY);
            if broken {
                self.trailing(self.last_end);
                self.newline();
            }
        }
        self.group_end(broken, Some(l.close), g);
        if !broken {
            self.push(" ");
        }
        self.push("}");
    }

    // ---- leaves ----

    fn path(&mut self, p: &[String]) {
        for (i, seg) in p.iter().enumerate() {
            if i > 0 {
                self.push("::");
            }
            self.push(seg);
        }
    }

    fn use_prefix(&mut self, prefix: &[String]) {
        for seg in prefix {
            self.push(seg);
            self.push("::");
        }
    }

    fn use_tree(&mut self, tree: &UseTree) {
        match tree {
            UseTree::Leaf { path, alias } => {
                self.path(path);
                if let Some(a) = alias {
                    self.push(" as ");
                    self.push(a);
                }
            }
            UseTree::Glob { prefix } => {
                self.use_prefix(prefix);
                self.push("*");
            }
            UseTree::Group { prefix, children } => {
                self.use_prefix(prefix);
                self.push("{");
                for (i, c) in children.iter().enumerate() {
                    if i > 0 {
                        self.push(", ");
                    }
                    self.use_tree(c);
                }
                self.push("}");
            }
        }
    }

    /// The literal exactly as the author wrote it, when that text spells the
    /// same token. `0xff` is not `255` to a reader and `1_000.5` is not
    /// `1000.5`; the AST holds the value, so the spelling only survives if it is
    /// taken from the source.
    ///
    /// False when the span is not the literal alone — a negative literal in a
    /// pattern gets the whole `-1` — and the caller spells the value out.
    fn lit_verbatim(&mut self, span: &Span, token: &Token) -> bool {
        let written = self.src.get(span.start..span.end).unwrap_or_default();
        if matches!(lexer::lex(written).as_deref(), Ok([s]) if s.token == *token) {
            self.push(written);
            true
        } else {
            false
        }
    }

    /// A string with no interpolation — an annotation's argument, a test's name.
    fn str_verbatim(&mut self, after: usize) {
        let open = self.token_span(after, &Token::StrStart);
        let end = self.close_of(open.end, &Token::StrEnd);
        self.verbatim(&(open.start..end));
    }

    /// A string literal. The text is the author's, byte for byte: an escape is
    /// not the character it denotes, and a bare `{` is literal and must not grow
    /// an escape. Only the interpolation holes are formatted.
    fn str_parts(&mut self, parts: &[StrPart], span: &Span) {
        let mut cursor = span.start;
        for p in parts {
            if let StrPart::Interp(e) = p {
                let open = self.token_span(cursor, &Token::InterpStart);
                self.verbatim(&(cursor..open.start));
                self.push("#{");
                self.grouped(|s| s.expr(e, P_GREEDY));
                self.push("}");
                // A hole is brace-matched and may hold another string, so the
                // hole's own `}` is the first one past the whole expression.
                cursor = self.close_of(e.span.end, &Token::InterpEnd);
            }
        }
        self.verbatim(&(cursor..span.end));
    }
}

fn elem_expr(e: &Elem) -> &Expr {
    match e {
        Elem::Value(e) | Elem::Spread(e) => e,
    }
}

/// Mirrors the parser's `is_block_like`: these stand alone as statements with no
/// semicolon.
fn is_block_like(e: &Expr) -> bool {
    is_block_like_kind(&e.kind)
}

fn is_block_like_kind(e: &ExprKind) -> bool {
    matches!(
        e,
        ExprKind::If { .. }
            | ExprKind::Match { .. }
            | ExprKind::Loop { .. }
            | ExprKind::While { .. }
            | ExprKind::For { .. }
            | ExprKind::Block(_)
    )
}

fn expr_prec(e: &Expr) -> u8 {
    match &e.kind {
        ExprKind::Binary { op, .. } => op.prec(),
        ExprKind::Unary { .. } => P_UNARY,
        ExprKind::Call { .. }
        | ExprKind::Index { .. }
        | ExprKind::Field { .. }
        | ExprKind::Is { .. }
        | ExprKind::As { .. } => P_POSTFIX,
        // `break` and `return` with no value are greedy too: in `break - 1` the
        // `-1` is read as their operand.
        ExprKind::Return(_)
        | ExprKind::Break(_)
        | ExprKind::Throw(_)
        | ExprKind::Lambda { .. } => P_GREEDY,
        // `try` is NOT greedy: its body is the unary-level parser, so it binds
        // tighter than every binary operator. `try? get(m, k) orelse 30` is
        // `(try? get(m, k)) orelse 30` and needs no parentheses to say so.
        ExprKind::Try { .. } => P_UNARY,
        e if is_block_like_kind(e) => P_BLOCK_LIKE,
        _ => P_ATOM,
    }
}

fn type_prec(t: &TypeSpec) -> u8 {
    match &t.kind {
        TypeSpecKind::Union(_) => TP_UNION,
        TypeSpecKind::Intersect(_) => TP_INTERSECT,
        TypeSpecKind::Negate(_) => TP_NEGATE,
        TypeSpecKind::Fn { .. } => TP_FN,
        _ => TP_ATOM,
    }
}

/// The fallback spelling of a rune whose source text is not recoverable.
fn escape_char(c: char, quote: char, out: &mut String) {
    match c {
        '\\' => out.push_str("\\\\"),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        '\0' => out.push_str("\\0"),
        c if c == quote => {
            out.push('\\');
            out.push(c);
        }
        c if (c as u32) < 0x20 || c as u32 == 0x7f => {
            let _ = write!(out, "\\u{{{:x}}}", c as u32);
        }
        c => out.push(c),
    }
}
