mod error;

#[cfg(test)]
mod tests;

pub use error::{Expected, ParseError, ParseErrorKind, Span};

use crate::ast::*;
use crate::lexer::{Spanned, Token};
use chumsky::input::{Input, ValueInput};
use chumsky::pratt::{infix, left};
use chumsky::prelude::*;

type Extra = extra::Err<ParseError>;

pub fn parse(tokens: &[Spanned], eoi: usize) -> (Option<Module>, Vec<ParseError>) {
    let owned: Vec<(Token, Span)> = tokens
        .iter()
        .map(|s| (s.token.clone(), s.span.clone()))
        .collect();
    let input = owned.as_slice().map(eoi..eoi, |(t, s)| (t, s));
    // Bound rather than inlined: as a temporary the parser outlives `owned`,
    // which it borrows, and is dropped after it.
    let parser = module();
    parser.parse(input).into_output_errors()
}

/// Shorthand for the bound every rule below satisfies.
trait P<'t, I, O>: Parser<'t, I, O, Extra> + Clone
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
}
impl<'t, I, O, T> P<'t, I, O> for T
where
    I: ValueInput<'t, Token = Token, Span = Span>,
    T: Parser<'t, I, O, Extra> + Clone,
{
}

/// Where recovery stops skipping: a new declaration, or the `}` that ends the
/// enclosing module.
const DECL_STOP: [Token; 13] = [
    Token::Fn,
    Token::Test,
    Token::Bench,
    Token::Record,
    Token::Opaque,
    Token::Type,
    Token::Mu,
    Token::Newtype,
    Token::Protocol,
    Token::Impl,
    Token::Use,
    Token::Const,
    Token::RBrace,
];

/// The single construction site.
///
/// Every rule below takes the sub-parsers it needs rather than building them, so
/// the type grammar and the expression grammar are each constructed once. A rule
/// that calls `type_spec()` or `block()` gets a private copy of that whole
/// grammar; the expression grammar was being built five times and the type
/// grammar fourteen.
///
/// `.boxed()` throughout keeps each rule's type opaque; without it the
/// combinator types nest and compile time grows superlinearly.
fn module<'t, I>() -> impl P<'t, I, Module>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    let ty = type_spec();
    let (expr, block) = expr_and_block(ty.clone());

    decl(ty, expr, block)
        .repeated()
        .collect::<Vec<_>>()
        .map(|decls| Module { decls })
        .then_ignore(end())
        .boxed()
}

// ---- declarations ----

fn decl<'t, I>(
    ty: impl P<'t, I, TypeSpec> + 't,
    expr: impl P<'t, I, Expr> + 't,
    block: impl P<'t, I, Block> + 't,
) -> impl P<'t, I, Decl>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    recursive(|decl| {
        let inner = choice((
            fn_like(ty.clone(), block.clone(), true).map(DeclKind::Fn),
            record_decl(ty.clone()).map(DeclKind::Record),
            protocol_decl(ty.clone(), block.clone()).map(DeclKind::Protocol),
            impl_decl(ty.clone(), block.clone()).map(DeclKind::Impl),
            mu_type_decl(ty.clone()),
            type_alias_decl(ty.clone()),
            newtype_decl(ty.clone()),
            use_decl(),
            mod_decl(decl),
            const_decl(ty, expr),
            test_decl(block),
            enum_decl(),
        ))
        .map_with(|kind, e| Decl { kind, span: e.span() })
        .boxed();

        // Skipping a bad declaration has to respect braces two ways.
        //
        // A lone `}` must stop recovery: inside `mod m { .. }` it is what ends
        // the repetition, and eating it leaves the module unterminated. Hence
        // `none_of([RBrace])` on the first token and RBrace in DECL_STOP.
        //
        // But a *balanced* `{ .. }` is part of the declaration being discarded —
        // `fn broken( {}` has a body — so it is skipped as a unit. Without that,
        // recovery halts on the body's own `}` and everything after it is lost.
        //
        // The leading `none_of` also guarantees progress: recovery restarts from
        // where the declaration *began*, itself a decl-start token, so a
        // strategy that skips "until a decl start" would match immediately,
        // retry at the same token, fail identically, and get abandoned.
        let recovery = none_of([Token::RBrace])
            .then(choice((nested_braces(), none_of(DECL_STOP).ignored())).repeated())
            .map_with(|_, e| Decl { kind: DeclKind::Error, span: e.span() })
            .boxed();

        inner.recover_with(via_parser(recovery)).boxed()
    })
}

/// `@name` or `@name("string")`, and that is all: `@cfg("not(windows)")` rather
/// than a nested expression, so the grammar needs no expression language of its
/// own for a corner nobody reads.
fn annotations<'t, I>() -> impl P<'t, I, Vec<Annotation>>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    just(Token::At)
        .ignore_then(ident())
        .then(
            plain_str()
                .delimited_by(just(Token::LParen), just(Token::RParen))
                .or_not(),
        )
        .map_with(|(name, arg), e| Annotation { name, arg, span: e.span() })
        .repeated()
        .collect()
        .boxed()
}

/// `[T, U]` on a declaration.
fn generic_params<'t, I>() -> impl P<'t, I, Vec<String>>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    ident()
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .collect()
        .delimited_by(just(Token::LBracket), just(Token::RBracket))
        .or_not()
        .map(Option::unwrap_or_default)
        .boxed()
}

fn where_clauses<'t, I>(ty: impl P<'t, I, TypeSpec> + 't) -> impl P<'t, I, Vec<WhereClause>>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    just(Token::Where)
        .ignore_then(
            ident()
                .then_ignore(just(Token::Colon))
                .then(ty)
                .map(|(param, bound)| WhereClause { param, bound })
                .separated_by(just(Token::Comma))
                .at_least(1)
                .collect(),
        )
        .or_not()
        .map(Option::unwrap_or_default)
        .boxed()
}

/// A function. `body_required` is false for a protocol's methods, which may stop
/// at the signature or supply a default body.
fn fn_like<'t, I>(
    ty: impl P<'t, I, TypeSpec> + 't,
    block: impl P<'t, I, Block> + 't,
    body_required: bool,
) -> impl P<'t, I, FnDecl>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    let body = if body_required {
        block.map(Some).boxed()
    } else {
        block.or_not().boxed()
    };

    annotations()
        .then_ignore(just(Token::Fn))
        .then(ident())
        .then(generic_params())
        .then(
            param(ty.clone())
                .separated_by(just(Token::Comma))
                .allow_trailing()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LParen), just(Token::RParen)),
        )
        // `throws E` is written before `->`.
        .then(just(Token::Throws).ignore_then(ty.clone()).or_not())
        .then(just(Token::Arrow).ignore_then(ty.clone()).or_not())
        .then(where_clauses(ty))
        .then(body)
        .map(
            |(((((((annotations, name), generics), params), throws), ret), wheres), body)| FnDecl {
                name,
                generics,
                params,
                throws,
                ret,
                wheres,
                body,
                annotations,
            },
        )
        .boxed()
}

fn param<'t, I>(ty: impl P<'t, I, TypeSpec> + 't) -> impl P<'t, I, Param>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    ident()
        .then_ignore(just(Token::Colon))
        .then(ty)
        .map_with(|(name, ty), e| Param { name, ty, span: e.span() })
        .boxed()
}

fn field<'t, I>(ty: impl P<'t, I, TypeSpec> + 't) -> impl P<'t, I, Field>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    ident()
        .then_ignore(just(Token::Colon))
        .then(ty)
        .map_with(|(name, ty), e| Field { name, ty, span: e.span() })
        .boxed()
}

fn record_decl<'t, I>(ty: impl P<'t, I, TypeSpec> + 't) -> impl P<'t, I, RecordDecl>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    annotations()
        .then(just(Token::Opaque).or_not().map(|o| o.is_some()))
        .then_ignore(just(Token::Record))
        .then(ident())
        .then(generic_params())
        .then(
            field(ty)
                .separated_by(just(Token::Comma))
                .allow_trailing()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LBrace), just(Token::RBrace)),
        )
        .map(|((((annotations, opaque), name), generics), fields)| RecordDecl {
            name,
            generics,
            opaque,
            fields,
            annotations,
        })
        .boxed()
}

fn protocol_decl<'t, I>(
    ty: impl P<'t, I, TypeSpec> + 't,
    block: impl P<'t, I, Block> + 't,
) -> impl P<'t, I, ProtocolDecl>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    // The subject is a type or a type constructor: `for T` or `for C[_]`.
    let subject = ident()
        .then(
            ident_named("_")
                .separated_by(just(Token::Comma))
                .at_least(1)
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LBracket), just(Token::RBracket))
                .or_not()
                .map(|p| p.map_or(0, |v| v.len())),
        )
        .boxed();

    just(Token::Protocol)
        .ignore_then(ident())
        .then_ignore(just(Token::For))
        .then(subject)
        .then(
            fn_like(ty, block, false)
                .repeated()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LBrace), just(Token::RBrace)),
        )
        .map(|((name, (subject, subject_arity)), methods)| ProtocolDecl {
            name,
            subject,
            subject_arity,
            methods,
        })
        .boxed()
}

fn impl_decl<'t, I>(
    ty: impl P<'t, I, TypeSpec> + 't,
    block: impl P<'t, I, Block> + 't,
) -> impl P<'t, I, ImplDecl>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    just(Token::Impl)
        .ignore_then(generic_params())
        .then(path())
        .then_ignore(just(Token::For))
        .then(ty.clone())
        .then(
            fn_like(ty, block, true)
                .repeated()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LBrace), just(Token::RBrace)),
        )
        .map(|(((generics, protocol), target), methods)| ImplDecl {
            protocol,
            generics,
            target,
            methods,
        })
        .boxed()
}

/// The shared shape of `type`, `mu type` and `newtype`. No trailing semicolon.
fn alias_body<'t, I>(ty: impl P<'t, I, TypeSpec> + 't) -> impl P<'t, I, AliasDecl>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    ident()
        .then(generic_params())
        .then_ignore(just(Token::Eq))
        .then(ty)
        .map(|((name, generics), value)| AliasDecl { name, generics, value })
        .boxed()
}

fn type_alias_decl<'t, I>(ty: impl P<'t, I, TypeSpec> + 't) -> impl P<'t, I, DeclKind>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    just(Token::Type)
        .ignore_then(alias_body(ty))
        .map(DeclKind::TypeAlias)
        .boxed()
}

fn mu_type_decl<'t, I>(ty: impl P<'t, I, TypeSpec> + 't) -> impl P<'t, I, DeclKind>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    just(Token::Mu)
        .ignore_then(just(Token::Type))
        .ignore_then(alias_body(ty))
        .map(DeclKind::MuType)
        .boxed()
}

fn newtype_decl<'t, I>(ty: impl P<'t, I, TypeSpec> + 't) -> impl P<'t, I, DeclKind>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    just(Token::Newtype)
        .ignore_then(alias_body(ty))
        .map(DeclKind::Newtype)
        .boxed()
}

fn use_decl<'t, I>() -> impl P<'t, I, DeclKind>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    just(Token::Use)
        .ignore_then(path())
        .then_ignore(just(Token::Semi).or_not())
        .map_with(|path, e| DeclKind::Use(UsePath { path, span: e.span() }))
        .boxed()
}

fn mod_decl<'t, I>(decl: impl P<'t, I, Decl> + 't) -> impl P<'t, I, DeclKind>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    just(Token::Internal)
        .or_not()
        .map(|i| i.is_some())
        .then_ignore(just(Token::Mod))
        .then(ident())
        .then(
            decl.repeated()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LBrace), just(Token::RBrace)),
        )
        .map(|((internal, name), decls)| DeclKind::Mod(ModDecl { name, internal, decls }))
        .boxed()
}

fn const_decl<'t, I>(
    ty: impl P<'t, I, TypeSpec> + 't,
    expr: impl P<'t, I, Expr> + 't,
) -> impl P<'t, I, DeclKind>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    just(Token::Const)
        .ignore_then(ident())
        .then(just(Token::Colon).ignore_then(ty).or_not())
        .then_ignore(just(Token::Eq))
        .then(expr)
        .then_ignore(just(Token::Semi).or_not())
        .map(|((name, ty), value)| DeclKind::Const(ConstDecl { name, ty, value }))
        .boxed()
}

fn test_decl<'t, I>(block: impl P<'t, I, Block> + 't) -> impl P<'t, I, DeclKind>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    choice((
        just(Token::Test).to(TestKind::Test),
        just(Token::Bench).to(TestKind::Bench),
    ))
    .then(plain_str())
    .then(block)
    .map(|((kind, name), body)| DeclKind::TestBlock(TestBlock { kind, name, body }))
    .boxed()
}

/// `enum` lexes as an ordinary identifier. Catch it here and say what to do
/// instead: without this the user gets a cascade about an unexpected identifier,
/// which explains nothing.
fn enum_decl<'t, I>() -> impl P<'t, I, DeclKind>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    ident_named("enum")
        .then(ident())
        .then(nested_braces())
        .validate(|_, e, emitter| {
            emitter.emit(ParseError::new(e.span(), ParseErrorKind::EnumDeclaration));
            DeclKind::Error
        })
        .boxed()
}

// ---- types ----

fn type_spec<'t, I>() -> impl P<'t, I, TypeSpec>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    recursive(|ty| {
        let named = path()
            .then(
                ty.clone()
                    .separated_by(just(Token::Comma))
                    .allow_trailing()
                    .collect::<Vec<_>>()
                    .delimited_by(just(Token::LBracket), just(Token::RBracket))
                    .or_not()
                    .map(Option::unwrap_or_default),
            )
            .map(|(path, args)| TypeSpecKind::Named { path, args });

        let structural = field(ty.clone())
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .collect::<Vec<_>>()
            .delimited_by(just(Token::LBrace), just(Token::RBrace))
            .map(TypeSpecKind::Struct);

        // `(A, B) -> C` and `(A, B)`; `(A)` is just a grouping.
        let parenthesised = ty
            .clone()
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .collect::<Vec<_>>()
            .delimited_by(just(Token::LParen), just(Token::RParen))
            .then(just(Token::Arrow).ignore_then(ty.clone()).or_not())
            .map(|(items, ret)| match ret {
                Some(ret) => TypeSpecKind::Fn { params: items, ret: Box::new(ret) },
                None if items.len() == 1 => items.into_iter().next().expect("len 1").kind,
                None => TypeSpecKind::Tuple(items),
            });

        let atom = select! { Token::Atom(a) => TypeSpecKind::Atom(a) };
        let null = just(Token::Null).to(TypeSpecKind::Null);
        let any = ident_named("any").to(TypeSpecKind::Any);

        let atomic = choice((null, any, atom, structural, parenthesised, named))
            .map_with(|kind, e| TypeSpec { kind, span: e.span() })
            .boxed();

        // `!` binds tightest, then `&`, then `|`: a union of intersections is
        // the shape people mean.
        let negated = just(Token::Bang)
            .repeated()
            .foldr_with(atomic, |_, ty, e| TypeSpec {
                kind: TypeSpecKind::Negate(Box::new(ty)),
                span: e.span(),
            })
            .boxed();

        let intersect = negated
            .separated_by(just(Token::Ampersand))
            .at_least(1)
            .collect::<Vec<_>>()
            .map_with(|mut v, e| {
                if v.len() == 1 {
                    v.pop().expect("len 1")
                } else {
                    TypeSpec { kind: TypeSpecKind::Intersect(v), span: e.span() }
                }
            })
            .boxed();

        intersect
            .separated_by(just(Token::Bar))
            .at_least(1)
            .collect::<Vec<_>>()
            .map_with(|mut v, e| {
                if v.len() == 1 {
                    v.pop().expect("len 1")
                } else {
                    TypeSpec { kind: TypeSpecKind::Union(v), span: e.span() }
                }
            })
            .labelled("a type")
            .boxed()
    })
}

// ---- expressions, statements, blocks ----

/// One entry in a block, before deciding which is the tail.
enum Item {
    Stmt(Stmt),
    /// The bool is whether a `;` followed.
    Expr(Expr, bool),
}

/// Expressions that end in a block stand alone as statements, with no `;`.
fn is_block_like(e: &Expr) -> bool {
    matches!(
        e.kind,
        ExprKind::If { .. }
            | ExprKind::Match { .. }
            | ExprKind::Loop { .. }
            | ExprKind::While { .. }
            | ExprKind::For { .. }
            | ExprKind::Block(_)
    )
}

/// Expressions and blocks are mutually recursive: a block holds expressions, and
/// a block is itself an expression (`if`, `loop`, a bare `{ .. }`). `recursive`
/// closes one knot, so these need `declare`/`define` to close both. Building
/// them separately would have each call construct the other from scratch, which
/// is infinite recursion at construction time — a stack overflow before a single
/// token is read.
fn expr_and_block<'t, I>(
    ty: impl P<'t, I, TypeSpec> + 't,
) -> (impl P<'t, I, Expr>, impl P<'t, I, Block>)
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    let mut expr = Recursive::declare();
    let mut cond = Recursive::declare();
    let mut block = Recursive::declare();

    // A block is a run of items; the last one, if it is an expression with no
    // semicolon, is the block's value.
    //
    // This cannot be `stmt.repeated().then(expr.or_not())`: `repeated` is greedy,
    // so a trailing `if a { 1 } else { 2 }` would be consumed as a statement and
    // the block would lose its value. Collecting uniformly and deciding
    // afterwards is the only way to tell "last" from "not last".
    block.define(
        item(ty.clone(), expr.clone())
            .repeated()
            .collect::<Vec<_>>()
            .delimited_by(just(Token::LBrace), just(Token::RBrace))
            .validate(|items, e, emitter| {
                let mut stmts = Vec::new();
                let mut tail = None;
                let last = items.len().saturating_sub(1);
                for (i, item) in items.into_iter().enumerate() {
                    match item {
                        Item::Stmt(s) => stmts.push(s),
                        Item::Expr(x, semi) => {
                            if i == last && !semi {
                                tail = Some(Box::new(x));
                            } else {
                                // A block-like expression stands alone as a
                                // statement; anything else needs its semicolon.
                                if !semi && !is_block_like(&x) {
                                    emitter.emit(ParseError::new(
                                        x.span.end..x.span.end,
                                        ParseErrorKind::Expected {
                                            expected: vec![Expected::Token(Token::Semi)],
                                            found: None,
                                        },
                                    ));
                                }
                                let span = x.span.clone();
                                stmts.push(Stmt { kind: StmtKind::Expr(x), span });
                            }
                        }
                    }
                }
                Block { stmts, tail, span: e.span() }
            })
            .boxed(),
    );

    expr.define({
        let atom = atom_expr(expr.clone(), cond.clone(), block.clone(), ty.clone(), true).boxed();
        let postfixed = postfix_ops(atom, expr.clone(), ty.clone()).boxed();
        binary_ops(postfixed).boxed()
    });

    // The condition of `if`/`while`/`for` and a match scrutinee sit directly
    // before a `{`, so a record literal there is ambiguous with the body:
    // `while a { }` would read `a { }` as an empty record and then find no body.
    // Rust has the same problem and solves it the same way. Parenthesise to get
    // a record literal back: `while (a { }) { }`.
    //
    // This is a second copy of the expression grammar. Threading a flag instead
    // would mean a Context type parameter on every rule's Extra and a
    // consume-then-rewind in the record literal — more machinery than the copy
    // costs, now that the grammar is built once rather than five times.
    cond.define({
        let atom = atom_expr(expr.clone(), cond.clone(), block.clone(), ty.clone(), false).boxed();
        let postfixed = postfix_ops(atom, expr.clone(), ty).boxed();
        binary_ops(postfixed).boxed()
    });

    (expr, block)
}

/// One statement or expression inside a block.
///
/// The expression is parsed **once** and what follows decides what it was: `=`
/// makes it an assignment, `;` a statement, neither the block's tail. Trying
/// `expr =` and falling back to `expr ;` would parse the expression twice for
/// every statement that is not an assignment.
fn item<'t, I>(
    ty: impl P<'t, I, TypeSpec> + 't,
    expr: impl P<'t, I, Expr> + 't,
) -> impl P<'t, I, Item>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    let let_stmt = just(Token::Let)
        .ignore_then(pattern(ty.clone(), expr.clone()))
        .then(just(Token::Colon).ignore_then(ty).or_not())
        .then_ignore(just(Token::Eq))
        .then(expr.clone())
        .then_ignore(just(Token::Semi).or_not())
        .map_with(|((pat, ty), value), e| {
            Item::Stmt(Stmt { kind: StmtKind::Let { pat, ty, value }, span: e.span() })
        })
        .boxed();

    let rest = expr
        .clone()
        .then(just(Token::Eq).ignore_then(expr).or_not())
        .then(just(Token::Semi).or_not())
        .validate(|((target, assigned), semi), e, emitter| {
            let Some(value) = assigned else {
                return Item::Expr(target, semi.is_some());
            };
            // An assignment. Bindings rebind; there is no `mut`. The target was
            // parsed as a full expression only so `p.f = e` and `xs[i] = e` can
            // be rejected with a diagnostic saying what to write instead — both
            // are mutation, and records and lists are values, but people will
            // type them and a parse failure is a bad way to find that out.
            let kind = match target.kind {
                ExprKind::Path(mut segments) if segments.len() == 1 => {
                    StmtKind::Assign { name: segments.pop().expect("len 1"), value }
                }
                ExprKind::Field { .. } => {
                    emitter.emit(ParseError::new(target.span, ParseErrorKind::FieldAssignment));
                    StmtKind::Error
                }
                ExprKind::Index { .. } => {
                    emitter.emit(ParseError::new(target.span, ParseErrorKind::IndexAssignment));
                    StmtKind::Error
                }
                // A qualified name, or anything else: only a binding can be
                // rebound.
                _ => {
                    emitter
                        .emit(ParseError::new(target.span, ParseErrorKind::InvalidAssignTarget));
                    StmtKind::Error
                }
            };
            Item::Stmt(Stmt { kind, span: e.span() })
        })
        .boxed();

    choice((let_stmt, rest)).boxed()
}

// ---- patterns ----

fn pattern<'t, I>(
    ty: impl P<'t, I, TypeSpec> + 't,
    expr: impl P<'t, I, Expr> + 't,
) -> impl P<'t, I, Pattern>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    recursive(|pat| {
        let wildcard = ident_named("_").to(PatternKind::Wildcard);

        let is_pat = just(Token::Is).ignore_then(ty).map(PatternKind::Is);

        // `Point { x, y }` — `x` alone binds the field to `x`.
        let field_pat = ident()
            .then(just(Token::Colon).ignore_then(pat.clone()).or_not())
            .map_with(|(name, pat), e| FieldPat { name, pat, span: e.span() })
            .boxed();

        let record_pat = path()
            .or_not()
            .then(
                field_pat
                    .separated_by(just(Token::Comma))
                    .allow_trailing()
                    .collect::<Vec<_>>()
                    .then(just(Token::DotDot).or_not().map(|r| r.is_some()))
                    .delimited_by(just(Token::LBrace), just(Token::RBrace)),
            )
            .map(|(path, (fields, rest))| PatternKind::Record { path, fields, rest })
            .boxed();

        let tuple_pat = pat
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .collect::<Vec<_>>()
            .delimited_by(just(Token::LParen), just(Token::RParen))
            .map(PatternKind::Tuple)
            .boxed();

        let literal = literal_expr(expr)
            .map(|e| PatternKind::Literal(Box::new(e)))
            .boxed();

        let bind = ident().map(PatternKind::Bind);

        choice((wildcard, is_pat, record_pat, tuple_pat, literal, bind))
            .map_with(|kind, e| Pattern { kind, span: e.span() })
            .labelled("a pattern")
            .boxed()
    })
}

/// Literals only, for pattern position where a full expression would be wrong.
fn literal_expr<'t, I>(expr: impl P<'t, I, Expr> + 't) -> impl P<'t, I, Expr>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    let neg = just(Token::Minus)
        .ignore_then(select! {
            Token::Int(n) => ExprKind::Int(n),
            Token::Float(f) => ExprKind::Float(f),
        })
        .map_with(|kind, e| Expr {
            kind: ExprKind::Unary {
                op: UnOp::Neg,
                rhs: Box::new(Expr { kind, span: e.span() }),
            },
            span: e.span(),
        });

    choice((neg, literal_token(), string_expr(expr))).boxed()
}

/// The literal tokens, shared by expression and pattern position.
fn literal_token<'t, I>() -> impl P<'t, I, Expr>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    select! {
        Token::Int(n) => ExprKind::Int(n),
        Token::Float(f) => ExprKind::Float(f),
        Token::Rune(c) => ExprKind::Rune(c),
        Token::Atom(a) => ExprKind::Atom(a),
        Token::True => ExprKind::Bool(true),
        Token::False => ExprKind::Bool(false),
        Token::Null => ExprKind::Null,
    }
    .map_with(|kind, e| Expr { kind, span: e.span() })
}

/// Reassembles the lexer's flat string token run into parts.
fn string_expr<'t, I>(expr: impl P<'t, I, Expr> + 't) -> impl P<'t, I, Expr>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    let text = select! { Token::StrText(s) => StrPart::Text(s) };
    let interp = expr
        .delimited_by(just(Token::InterpStart), just(Token::InterpEnd))
        .map(StrPart::Interp);

    choice((text, interp))
        .repeated()
        .collect::<Vec<_>>()
        .delimited_by(just(Token::StrStart), just(Token::StrEnd))
        .map_with(|parts, e| Expr { kind: ExprKind::Str(parts), span: e.span() })
        .boxed()
}

/// Primary expressions.
fn atom_expr<'t, I>(
    expr: impl P<'t, I, Expr> + 't,
    cond: impl P<'t, I, Expr> + 't,
    block: impl P<'t, I, Block> + 't,
    ty: impl P<'t, I, TypeSpec> + 't,
    allow_record_lit: bool,
) -> impl P<'t, I, Expr>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    let paren = expr
        .clone()
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .collect::<Vec<_>>()
        .delimited_by(just(Token::LParen), just(Token::RParen))
        .map(|items| {
            if items.len() == 1 {
                items.into_iter().next().expect("len 1").kind
            } else {
                ExprKind::Tuple(items)
            }
        })
        .boxed();

    let list = choice((
        just(Token::DotDot).ignore_then(expr.clone()).map(Elem::Spread),
        expr.clone().map(Elem::Value),
    ))
    .separated_by(just(Token::Comma))
    .allow_trailing()
    .collect::<Vec<_>>()
    .delimited_by(just(Token::LBracket), just(Token::RBracket))
    .map(ExprKind::List)
    .boxed();

    let field_init = ident()
        .then_ignore(just(Token::Colon))
        .then(expr.clone())
        .map_with(|(name, value), e| FieldInit { name, value, span: e.span() })
        .boxed();

    // `Point { x: 1, ..base }`, or `{ x: 1 }` with no path.
    let record_lit = path()
        .or_not()
        .then(
            field_init
                .separated_by(just(Token::Comma))
                .allow_trailing()
                .collect::<Vec<_>>()
                .then(just(Token::DotDot).ignore_then(expr.clone()).or_not())
                .delimited_by(just(Token::LBrace), just(Token::RBrace)),
        )
        .map(|(path, (fields, spread))| ExprKind::RecordLit {
            path,
            fields,
            spread: spread.map(Box::new),
        })
        // In condition position a record literal is indistinguishable from the
        // body that follows, so it is switched off rather than guessed at.
        .filter(move |_| allow_record_lit)
        .boxed();

    let lambda = ident()
        .then(just(Token::Colon).ignore_then(ty.clone()).or_not())
        .map(|(name, ty)| LambdaParam { name, ty })
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .collect::<Vec<_>>()
        .delimited_by(just(Token::LParen), just(Token::RParen))
        .then_ignore(just(Token::FatArrow))
        .then(expr.clone())
        .map(|(params, body)| ExprKind::Lambda { params, body: Box::new(body) })
        .boxed();

    let if_expr = if_chain(cond.clone(), block.clone()).boxed();

    let match_expr = just(Token::Match)
        .ignore_then(cond.clone())
        .then(
            pattern(ty.clone(), expr.clone())
                .then(just(Token::If).ignore_then(expr.clone()).or_not())
                .then_ignore(just(Token::FatArrow))
                .then(expr.clone())
                .map_with(|((pat, guard), body), e| MatchArm { pat, guard, body, span: e.span() })
                .separated_by(just(Token::Comma))
                .allow_trailing()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LBrace), just(Token::RBrace)),
        )
        .map(|(scrutinee, arms)| ExprKind::Match { scrutinee: Box::new(scrutinee), arms })
        .boxed();

    let loop_expr = just(Token::Loop)
        .ignore_then(block.clone())
        .map(|body| ExprKind::Loop { body })
        .boxed();

    let while_expr = just(Token::While)
        .ignore_then(cond.clone())
        .then(block.clone())
        .map(|(cond, body)| ExprKind::While { cond: Box::new(cond), body })
        .boxed();

    let for_expr = just(Token::For)
        .ignore_then(pattern(ty, expr.clone()))
        .then_ignore(just(Token::In))
        .then(cond)
        .then(block.clone())
        .map(|((pat, iter), body)| ExprKind::For { pat, iter: Box::new(iter), body })
        .boxed();

    let break_expr = just(Token::Break)
        .ignore_then(expr.clone().or_not())
        .map(|v| ExprKind::Break(v.map(Box::new)))
        .boxed();
    let continue_expr = just(Token::Continue).to(ExprKind::Continue);
    let return_expr = just(Token::Return)
        .ignore_then(expr.clone().or_not())
        .map(|v| ExprKind::Return(v.map(Box::new)))
        .boxed();
    let throw_expr = just(Token::Throw)
        .ignore_then(expr.clone())
        .map(|v| ExprKind::Throw(Box::new(v)))
        .boxed();

    let try_expr = try_forms(expr.clone(), block.clone()).boxed();

    let assert_expr = choice((
        just(Token::Assert).to(AssertKind::Assert),
        just(Token::AssertEq).to(AssertKind::Eq),
        just(Token::AssertNe).to(AssertKind::Ne),
        just(Token::AssertThrows).to(AssertKind::Throws),
    ))
    .then(
        expr.clone()
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .collect::<Vec<_>>()
            .delimited_by(just(Token::LParen), just(Token::RParen)),
    )
    .map(|(kind, args)| ExprKind::Assert { kind, args })
    .boxed();

    let block_expr = block.map(ExprKind::Block).boxed();
    let path_expr = path().map(ExprKind::Path).boxed();

    let unary = choice((
        just(Token::Minus).to(UnOp::Neg),
        just(Token::Bang).to(UnOp::Not),
        just(Token::Bnot).to(UnOp::Bnot),
    ));

    let core = choice((
        // Keyword-led forms first: they can never be a path.
        if_expr,
        match_expr,
        loop_expr,
        while_expr,
        for_expr,
        break_expr,
        continue_expr,
        return_expr,
        throw_expr,
        try_expr,
        assert_expr,
        // `(x) => e` before a parenthesised expression, since both start `(`.
        lambda,
        paren,
        list,
        // A record literal needs a path or `{`; try it before a bare path so
        // `Point { x: 1 }` is not read as `Point` followed by a block.
        record_lit,
        block_expr,
        string_expr(expr).map(|e| e.kind).boxed(),
        literal_token().map(|e| e.kind).boxed(),
        path_expr,
    ))
    .map_with(|kind, e| Expr { kind, span: e.span() })
    .labelled("an expression")
    .boxed();

    unary
        .repeated()
        .foldr_with(core, |op, rhs, e| Expr {
            kind: ExprKind::Unary { op, rhs: Box::new(rhs) },
            span: e.span(),
        })
        .boxed()
}

/// `if c { .. } else if d { .. } else { .. }`. `else` is optional here; whether
/// it is *required* depends on the position the value is consumed in, which the
/// parser records rather than deciding.
fn if_chain<'t, I>(
    cond: impl P<'t, I, Expr> + 't,
    block: impl P<'t, I, Block> + 't,
) -> impl P<'t, I, ExprKind>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    recursive(|if_chain| {
        just(Token::If)
            .ignore_then(cond)
            .then(block.clone())
            .then(
                just(Token::Else)
                    .ignore_then(choice((
                        if_chain
                            .map_with(|kind, e| Expr { kind, span: e.span() })
                            .boxed(),
                        block
                            .map_with(|b, e| Expr { kind: ExprKind::Block(b), span: e.span() })
                            .boxed(),
                    )))
                    .or_not(),
            )
            .map(|((cond, then), else_)| ExprKind::If {
                cond: Box::new(cond),
                then,
                else_: else_.map(Box::new),
            })
            .boxed()
    })
}

/// `try e`, `try? e`, `try! e`, `try e catch (x) { .. }`. All forms accept a
/// block, so `try { a(); b() } catch (e) { .. }` covers every throwing call
/// inside.
fn try_forms<'t, I>(
    expr: impl P<'t, I, Expr> + 't,
    block: impl P<'t, I, Block> + 't,
) -> impl P<'t, I, ExprKind>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    just(Token::Try)
        .ignore_then(choice((
            just(Token::Question).to(TryForm::Soften),
            just(Token::Bang).to(TryForm::Assert),
            empty().to(TryForm::Propagate),
        )))
        .then(expr)
        .then(
            just(Token::Catch)
                .ignore_then(ident().delimited_by(just(Token::LParen), just(Token::RParen)))
                .then(block)
                .map_with(|(binding, body), e| CatchArm { binding, body, span: e.span() })
                .or_not(),
        )
        .map(|((form, body), catch)| ExprKind::Try { form, body: Box::new(body), catch })
        .boxed()
}

/// Call, index, field access, `is` and `as` — all bind tighter than any binary
/// operator.
fn postfix_ops<'t, I>(
    atom: impl P<'t, I, Expr> + 't,
    expr: impl P<'t, I, Expr> + 't,
    ty: impl P<'t, I, TypeSpec> + 't,
) -> impl P<'t, I, Expr>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    enum Post {
        Call(Vec<TypeSpec>, Vec<Expr>),
        Index(Expr),
        Field(String),
        Is(TypeSpec),
        As(TypeSpec),
    }

    // Turbofish: `f[i64](x)`.
    let generics = ty
        .clone()
        .separated_by(just(Token::Comma))
        .at_least(1)
        .collect::<Vec<_>>()
        .delimited_by(just(Token::LBracket), just(Token::RBracket))
        .or_not()
        .map(Option::unwrap_or_default)
        .boxed();

    let call = generics
        .then(
            expr.clone()
                .separated_by(just(Token::Comma))
                .allow_trailing()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LParen), just(Token::RParen)),
        )
        .map(|(g, args)| Post::Call(g, args))
        .boxed();

    let index = expr
        .delimited_by(just(Token::LBracket), just(Token::RBracket))
        .map(Post::Index)
        .boxed();

    let field = just(Token::Dot).ignore_then(ident()).map(Post::Field).boxed();
    let is_op = just(Token::Is).ignore_then(ty.clone()).map(Post::Is).boxed();
    let as_op = just(Token::As).ignore_then(ty).map(Post::As).boxed();

    atom.foldl_with(
        choice((call, index, field, is_op, as_op)).repeated(),
        |lhs, post, e| {
            let kind = match post {
                Post::Call(generics, args) => {
                    ExprKind::Call { callee: Box::new(lhs), generics, args }
                }
                Post::Index(index) => {
                    ExprKind::Index { base: Box::new(lhs), index: Box::new(index) }
                }
                Post::Field(name) => ExprKind::Field { base: Box::new(lhs), name },
                Post::Is(ty) => ExprKind::Is { lhs: Box::new(lhs), ty },
                Post::As(ty) => ExprKind::As { lhs: Box::new(lhs), ty },
            };
            Expr { kind, span: e.span() }
        },
    )
    .boxed()
}

/// The precedence ladder, loosest first.
///
/// `and` binds tighter than `or`. `|>` binds tighter than comparison — a pipe is
/// a call, and `x |> f() == 3` means `(x |> f()) == 3`; piping into a comparison
/// could never be a valid pipe target.
///
/// One entry per precedence level, not per operator: the op parser yields the
/// BinOp, so same-precedence operators collapse into a `choice`. Chumsky only
/// implements Operator for tuples up to a fixed arity, and a level-per-entry
/// table keeps the ladder readable anyway.
fn binary_ops<'t, I>(atom: impl P<'t, I, Expr> + 't) -> impl P<'t, I, Expr>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    // Captures nothing, so it is Copy and the same closure serves every level.
    // MapExtra is spelled out rather than inferred: with `&mut _` the closure's
    // type gets fixed before I and E are known, and the Operator impl then has
    // nothing to unify against.
    let fold = |lhs: Expr,
                op: BinOp,
                rhs: Expr,
                _e: &mut chumsky::input::MapExtra<'t, '_, I, Extra>|
     -> Expr {
        let span = lhs.span.start..rhs.span.end;
        Expr {
            kind: ExprKind::Binary { op, lhs: Box::new(lhs), rhs: Box::new(rhs) },
            span,
        }
    };

    let cmp = choice((
        just(Token::EqEq).to(BinOp::Eq),
        just(Token::NotEq).to(BinOp::Ne),
        just(Token::LtEq).to(BinOp::Le),
        just(Token::GtEq).to(BinOp::Ge),
        just(Token::Lt).to(BinOp::Lt),
        just(Token::Gt).to(BinOp::Gt),
    ));
    let shift = choice((
        just(Token::Bsl).to(BinOp::Bsl),
        just(Token::Bsr).to(BinOp::Bsr),
    ));
    let additive = choice((
        just(Token::Plus).to(BinOp::Add),
        just(Token::Minus).to(BinOp::Sub),
    ));
    let multiplicative = choice((
        just(Token::Star).to(BinOp::Mul),
        just(Token::Slash).to(BinOp::Div),
        just(Token::Percent).to(BinOp::Rem),
    ));

    atom.pratt((
        infix(left(1), just(Token::Orelse).to(BinOp::Orelse), fold),
        infix(left(2), just(Token::Or).to(BinOp::Or), fold),
        infix(left(3), just(Token::And).to(BinOp::And), fold),
        infix(left(4), cmp, fold),
        infix(left(5), just(Token::Pipe).to(BinOp::Pipe), fold),
        infix(left(6), just(Token::Bor).to(BinOp::Bor), fold),
        infix(left(7), just(Token::Bxor).to(BinOp::Bxor), fold),
        infix(left(8), just(Token::Band).to(BinOp::Band), fold),
        infix(left(9), shift, fold),
        infix(left(10), additive, fold),
        infix(left(11), multiplicative, fold),
    ))
    .boxed()
}

// ---- leaves ----

fn path<'t, I>() -> impl P<'t, I, Vec<String>>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    ident()
        .separated_by(just(Token::ColonColon))
        .at_least(1)
        .collect()
        .boxed()
}

fn ident<'t, I>() -> impl P<'t, I, String>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    select! { Token::Ident(name) => name }.labelled("an identifier")
}

/// A specific identifier. `enum`, `any` and `_` are not keywords, so they arrive
/// as idents and are matched by text.
fn ident_named<'t, I>(want: &'static str) -> impl P<'t, I, ()>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    select! { Token::Ident(name) if name == want => () }
}

/// A string with no interpolation.
fn plain_str<'t, I>() -> impl P<'t, I, String>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    just(Token::StrStart)
        .ignore_then(select! { Token::StrText(s) => s }.or_not())
        .then_ignore(just(Token::StrEnd))
        .map(Option::unwrap_or_default)
        .labelled("a string")
        .boxed()
}

/// A balanced `{ ... }` run, consumed and discarded.
fn nested_braces<'t, I>() -> impl P<'t, I, ()>
where
    I: ValueInput<'t, Token = Token, Span = Span>,
{
    recursive(|inner| {
        choice((
            inner
                .clone()
                .delimited_by(just(Token::LBrace), just(Token::RBrace)),
            none_of([Token::LBrace, Token::RBrace]).ignored(),
        ))
        .repeated()
        .ignored()
    })
    .delimited_by(just(Token::LBrace), just(Token::RBrace))
    .boxed()
}
