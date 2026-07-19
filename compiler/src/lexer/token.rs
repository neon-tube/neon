use std::fmt;
use std::ops::Range;

pub type Span = Range<usize>;

#[derive(Debug, Clone, PartialEq)]
pub struct Spanned {
    pub token: Token,
    pub span: Span,
}

/// A string literal lexes to a flat run of tokens rather than one value, because
/// interpolation nests arbitrarily — `"a #{f("b")} c"` puts a string inside a
/// string. So `"a #{x} b"` is:
///
/// ```text
/// StrStart StrText("a ") InterpStart Ident("x") InterpEnd StrText(" b") StrEnd
/// ```
///
/// The parser reassembles the tree. Nesting and quotes-inside-holes then need no
/// special cases anywhere.
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    // Declarations
    Fn,
    Let,
    Record,
    Opaque,
    Newtype,
    Type,
    /// `mu type A = ...` — an explicit recursive-type binder.
    Mu,
    Protocol,
    /// `marker Ord` — a bound with no methods, satisfied by a compiler rule
    /// rather than by an impl.
    Marker,
    Impl,
    Where,
    Use,
    Mod,
    Internal,
    Const,

    // Control flow
    If,
    Else,
    Match,
    Loop,
    While,
    For,
    In,
    Break,
    Continue,
    Return,

    // Errors
    Throws,
    Throw,
    Try,
    Catch,
    Orelse,

    // Tests
    Test,
    Bench,
    Assert,
    AssertEq,
    AssertNe,
    AssertThrows,

    // Operators that are words
    And,
    Or,
    Band,
    Bor,
    Bxor,
    Bnot,
    Bsl,
    Bsr,
    Is,
    As,

    // Literals that are words
    Null,
    True,
    False,

    // Punctuation
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Eq,
    EqEq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Arrow,
    FatArrow,
    Pipe,
    /// `..` — record and list spread. There is no `..=`: ranges are `range(a, b)`.
    DotDot,
    Question,
    Bang,
    Colon,
    ColonColon,
    Comma,
    Dot,
    Semi,
    At,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Ampersand,
    Bar,

    // Values
    Ident(String),
    Atom(String),
    /// The *magnitude*, unsigned. `-9223372036854775808` is a unary minus applied
    /// to 9223372036854775808, which does not fit an i64 — so the sign must be
    /// folded by the parser, not the lexer. Storing i64 here is what made
    /// `i64::MIN` unwritable in the previous implementation.
    Int(u64),
    /// Kept as text so the token can derive Eq/Hash; parsed later.
    Float(String),
    Rune(char),

    // Strings
    StrStart,
    StrText(String),
    InterpStart,
    InterpEnd,
    StrEnd,
}

impl Token {
    /// The keyword for an identifier, if it is one.
    ///
    /// `enum` is deliberately absent: it is not a keyword, so `enum` is an
    /// ordinary identifier. Sum types are unions of records. The parser reports a
    /// dedicated diagnostic when it sees `enum` used as a declaration, because
    /// people will type it.
    pub fn keyword(word: &str) -> Option<Token> {
        Some(match word {
            "fn" => Token::Fn,
            "let" => Token::Let,
            "record" => Token::Record,
            "opaque" => Token::Opaque,
            "newtype" => Token::Newtype,
            "type" => Token::Type,
            "mu" => Token::Mu,
            "protocol" => Token::Protocol,
            "marker" => Token::Marker,
            "impl" => Token::Impl,
            "where" => Token::Where,
            "use" => Token::Use,
            "mod" => Token::Mod,
            "internal" => Token::Internal,
            "const" => Token::Const,

            "if" => Token::If,
            "else" => Token::Else,
            "match" => Token::Match,
            "loop" => Token::Loop,
            "while" => Token::While,
            "for" => Token::For,
            "in" => Token::In,
            "break" => Token::Break,
            "continue" => Token::Continue,
            "return" => Token::Return,

            "throws" => Token::Throws,
            "throw" => Token::Throw,
            "try" => Token::Try,
            "catch" => Token::Catch,
            "orelse" => Token::Orelse,

            "test" => Token::Test,
            "bench" => Token::Bench,
            "assert" => Token::Assert,
            "assert_eq" => Token::AssertEq,
            "assert_ne" => Token::AssertNe,
            "assert_throws" => Token::AssertThrows,

            "and" => Token::And,
            "or" => Token::Or,
            "band" => Token::Band,
            "bor" => Token::Bor,
            "bxor" => Token::Bxor,
            "bnot" => Token::Bnot,
            "bsl" => Token::Bsl,
            "bsr" => Token::Bsr,
            "is" => Token::Is,
            "as" => Token::As,

            "null" => Token::Null,
            "true" => Token::True,
            "false" => Token::False,

            _ => return None,
        })
    }
}

impl fmt::Display for Token {
    /// How a token is named in a diagnostic.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Token::Fn => "fn",
            Token::Let => "let",
            Token::Record => "record",
            Token::Opaque => "opaque",
            Token::Newtype => "newtype",
            Token::Type => "type",
            Token::Mu => "mu",
            Token::Protocol => "protocol",
            Token::Marker => "marker",
            Token::Impl => "impl",
            Token::Where => "where",
            Token::Use => "use",
            Token::Mod => "mod",
            Token::Internal => "internal",
            Token::Const => "const",
            Token::If => "if",
            Token::Else => "else",
            Token::Match => "match",
            Token::Loop => "loop",
            Token::While => "while",
            Token::For => "for",
            Token::In => "in",
            Token::Break => "break",
            Token::Continue => "continue",
            Token::Return => "return",
            Token::Throws => "throws",
            Token::Throw => "throw",
            Token::Try => "try",
            Token::Catch => "catch",
            Token::Orelse => "orelse",
            Token::Test => "test",
            Token::Bench => "bench",
            Token::Assert => "assert",
            Token::AssertEq => "assert_eq",
            Token::AssertNe => "assert_ne",
            Token::AssertThrows => "assert_throws",
            Token::And => "and",
            Token::Or => "or",
            Token::Band => "band",
            Token::Bor => "bor",
            Token::Bxor => "bxor",
            Token::Bnot => "bnot",
            Token::Bsl => "bsl",
            Token::Bsr => "bsr",
            Token::Is => "is",
            Token::As => "as",
            Token::Null => "null",
            Token::True => "true",
            Token::False => "false",
            Token::Plus => "+",
            Token::Minus => "-",
            Token::Star => "*",
            Token::Slash => "/",
            Token::Percent => "%",
            Token::Eq => "=",
            Token::EqEq => "==",
            Token::NotEq => "!=",
            Token::Lt => "<",
            Token::LtEq => "<=",
            Token::Gt => ">",
            Token::GtEq => ">=",
            Token::Arrow => "->",
            Token::FatArrow => "=>",
            Token::Pipe => "|>",
            Token::DotDot => "..",
            Token::Question => "?",
            Token::Bang => "!",
            Token::Colon => ":",
            Token::ColonColon => "::",
            Token::Comma => ",",
            Token::Dot => ".",
            Token::Semi => ";",
            Token::At => "@",
            Token::LParen => "(",
            Token::RParen => ")",
            Token::LBracket => "[",
            Token::RBracket => "]",
            Token::LBrace => "{",
            Token::RBrace => "}",
            Token::Ampersand => "&",
            Token::Bar => "|",
            Token::Ident(s) => return write!(f, "identifier `{s}`"),
            Token::Atom(s) => return write!(f, "atom `:{s}`"),
            Token::Int(n) => return write!(f, "integer `{n}`"),
            Token::Float(s) => return write!(f, "float `{s}`"),
            Token::Rune(c) => return write!(f, "rune `{c}`"),
            Token::StrStart | Token::StrEnd => "`\"`",
            Token::StrText(_) => "string text",
            Token::InterpStart => "`#{`",
            Token::InterpEnd => "`}`",
        };
        write!(f, "`{s}`")
    }
}
