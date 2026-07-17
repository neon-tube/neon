use crate::lexer::Token;
use chumsky::error::Error as ChumskyError;
use chumsky::input::Input;
use chumsky::label::LabelError;
use chumsky::util::MaybeRef;
use chumsky::DefaultExpected;
use std::fmt;

pub use crate::lexer::Span;

/// A parse error.
///
/// Concrete rather than `Rich`: the kinds we raise deliberately are a closed
/// set, so a diagnostics pass can match on them and render each properly
/// instead of pattern-matching prose.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub span: Span,
    pub kind: ParseErrorKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParseErrorKind {
    /// Raised by the combinator machinery when nothing matched.
    Expected { expected: Vec<Expected>, found: Option<Token> },
    /// `enum Color { ... }` — not a keyword; sum types are unions of records.
    /// People will type it, so it gets a real diagnostic rather than a
    /// confusing cascade from `enum` lexing as an identifier.
    EnumDeclaration,
    /// `fn main() -> i64` / `fn main() throws E` — main's signature is fixed.
    MainSignatureFixed,
    /// `p.f = e` — records are values; there is no field assignment.
    FieldAssignment,
    /// `xs[i] = e` — lists and maps are values; there is no index assignment.
    IndexAssignment,
    /// `f() = e` and friends: the target is not something that can be rebound.
    InvalidAssignTarget,
}

/// What the parser wanted. Mirrors chumsky's `DefaultExpected`, but owned and
/// closed over our token type so it can outlive the parse.
#[derive(Debug, Clone, PartialEq)]
pub enum Expected {
    Token(Token),
    /// A named construct, from `.labelled("...")`.
    Label(&'static str),
    EndOfInput,
    /// Anything at all — the parser had no more specific expectation.
    Something,
}

impl ParseError {
    pub fn new(span: Span, kind: ParseErrorKind) -> Self {
        ParseError { span, kind }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ParseErrorKind::Expected { expected, found } => {
                write!(f, "expected ")?;
                write_expected(f, expected)?;
                match found {
                    Some(t) => write!(f, ", found {t}"),
                    None => write!(f, ", found end of input"),
                }
            }
            ParseErrorKind::EnumDeclaration => write!(
                f,
                "`enum` does not exist: declare the variants as records and union them, \
                 e.g. `record Red {{}}` `record Green {{}}` `type Color = Red | Green`"
            ),
            ParseErrorKind::MainSignatureFixed => write!(
                f,
                "`main` takes no return type and no `throws` clause: it returns `()` and \
                 implicitly throws `Error`. Use `std::exit(n)` for an exit code"
            ),
            ParseErrorKind::FieldAssignment => write!(
                f,
                "records are values: there is no field assignment. \
                 Build a new record instead, e.g. `p = Point {{ ..p, x: 1 }}`"
            ),
            ParseErrorKind::IndexAssignment => write!(
                f,
                "lists and maps are values: there is no index assignment. \
                 Use the returned copy instead, e.g. `xs = list::set(xs, i, v)`"
            ),
            ParseErrorKind::InvalidAssignTarget => {
                write!(f, "cannot assign to this: only a binding can be rebound")
            }
        }
    }
}

fn write_expected(f: &mut fmt::Formatter<'_>, expected: &[Expected]) -> fmt::Result {
    // Deduplicate while keeping order: the machinery repeats alternatives.
    let mut seen: Vec<String> = Vec::new();
    for e in expected {
        let s = match e {
            Expected::Token(t) => t.to_string(),
            Expected::Label(l) => l.to_string(),
            Expected::EndOfInput => "end of input".to_string(),
            Expected::Something => "something".to_string(),
        };
        if !seen.contains(&s) {
            seen.push(s);
        }
    }
    match seen.len() {
        0 => write!(f, "something else"),
        1 => write!(f, "{}", seen[0]),
        _ => {
            let last = seen.pop().expect("len > 1");
            write!(f, "{} or {}", seen.join(", "), last)
        }
    }
}

impl<'a, I> LabelError<'a, I, DefaultExpected<'a, Token>> for ParseError
where
    I: Input<'a, Token = Token, Span = Span>,
{
    fn expected_found<E: IntoIterator<Item = DefaultExpected<'a, Token>>>(
        expected: E,
        found: Option<MaybeRef<'a, Token>>,
        span: Span,
    ) -> Self {
        ParseError::new(
            span,
            ParseErrorKind::Expected {
                expected: expected
                    .into_iter()
                    .map(|e| match e {
                        DefaultExpected::Token(t) => Expected::Token(t.into_inner()),
                        DefaultExpected::EndOfInput => Expected::EndOfInput,
                        _ => Expected::Something,
                    })
                    .collect(),
                found: found.map(|f| f.into_inner()),
            },
        )
    }
}

impl<'a, I> LabelError<'a, I, &'static str> for ParseError
where
    I: Input<'a, Token = Token, Span = Span>,
{
    fn expected_found<E: IntoIterator<Item = &'static str>>(
        expected: E,
        found: Option<MaybeRef<'a, Token>>,
        span: Span,
    ) -> Self {
        ParseError::new(
            span,
            ParseErrorKind::Expected {
                expected: expected.into_iter().map(Expected::Label).collect(),
                found: found.map(|f| f.into_inner()),
            },
        )
    }

    /// `.labelled("a type")` should *replace* the raw token alternatives it
    /// covers, not add to them — that is the whole point of naming a construct.
    fn label_with(&mut self, label: &'static str) {
        if let ParseErrorKind::Expected { expected, .. } = &mut self.kind {
            *expected = vec![Expected::Label(label)];
        }
    }
}

impl<'a, I> ChumskyError<'a, I> for ParseError
where
    I: Input<'a, Token = Token, Span = Span>,
{
    fn merge(mut self, other: Self) -> Self {
        // Two errors at the same position: keep both sets of expectations, so
        // "expected `)` or `,`" rather than whichever alternative was tried last.
        // A deliberate diagnostic always wins over a generic one.
        match (&mut self.kind, &other.kind) {
            (ParseErrorKind::Expected { expected, .. }, ParseErrorKind::Expected { expected: b, .. }) => {
                expected.extend(b.iter().cloned());
                self
            }
            (ParseErrorKind::Expected { .. }, _) => other,
            _ => self,
        }
    }
}
