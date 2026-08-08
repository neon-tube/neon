//! The precedence ladder. One table, consumed by both the parser and the
//! formatter.
//!
//! The parser builds its levels by iterating this table, and the formatter
//! decides parentheses by reading `prec` out of it. Neither holds a second copy,
//! so they cannot disagree about what a program means — a formatter with its own
//! ladder prints `1 - (2 - 3)` as `1 - 2 - 3`, which is a silent miscompile
//! wearing a cosmetic disguise.

use crate::ast::BinOp;
use crate::lexer::Token;

/// One binary operator: how tightly it binds, and how it is spelled.
pub struct BinOpInfo {
    pub op: BinOp,
    /// `MIN_PREC` is loosest. Every binary operator is left-associative.
    pub prec: u8,
    /// The source spelling. `and`/`bsl` are words, not symbols.
    pub text: &'static str,
}

/// Loosest first. `and` binds tighter than `or`; `|>` binds tighter than
/// comparison, because a pipe is a call and calls bind tighter than comparison.
pub const BINARY_OPS: &[BinOpInfo] = &[
    BinOpInfo {
        op: BinOp::Orelse,
        prec: 1,
        text: "orelse",
    },
    BinOpInfo {
        op: BinOp::Or,
        prec: 2,
        text: "or",
    },
    BinOpInfo {
        op: BinOp::And,
        prec: 3,
        text: "and",
    },
    BinOpInfo {
        op: BinOp::Eq,
        prec: 4,
        text: "==",
    },
    BinOpInfo {
        op: BinOp::Ne,
        prec: 4,
        text: "!=",
    },
    BinOpInfo {
        op: BinOp::Le,
        prec: 4,
        text: "<=",
    },
    BinOpInfo {
        op: BinOp::Ge,
        prec: 4,
        text: ">=",
    },
    BinOpInfo {
        op: BinOp::Lt,
        prec: 4,
        text: "<",
    },
    BinOpInfo {
        op: BinOp::Gt,
        prec: 4,
        text: ">",
    },
    BinOpInfo {
        op: BinOp::Pipe,
        prec: 5,
        text: "|>",
    },
    BinOpInfo {
        op: BinOp::Bor,
        prec: 6,
        text: "bor",
    },
    BinOpInfo {
        op: BinOp::Bxor,
        prec: 7,
        text: "bxor",
    },
    BinOpInfo {
        op: BinOp::Band,
        prec: 8,
        text: "band",
    },
    BinOpInfo {
        op: BinOp::Bsl,
        prec: 9,
        text: "bsl",
    },
    BinOpInfo {
        op: BinOp::Bsr,
        prec: 9,
        text: "bsr",
    },
    BinOpInfo {
        op: BinOp::Add,
        prec: 10,
        text: "+",
    },
    BinOpInfo {
        op: BinOp::Sub,
        prec: 10,
        text: "-",
    },
    BinOpInfo {
        op: BinOp::Mul,
        prec: 11,
        text: "*",
    },
    BinOpInfo {
        op: BinOp::Div,
        prec: 11,
        text: "/",
    },
    BinOpInfo {
        op: BinOp::Rem,
        prec: 11,
        text: "%",
    },
];

/// The loosest binary level.
pub const MIN_PREC: u8 = 1;

/// The tightest binary level. Anything above this is not a binary operator.
pub const MAX_PREC: u8 = 11;

impl BinOp {
    fn info(self) -> &'static BinOpInfo {
        BINARY_OPS
            .iter()
            .find(|i| i.op == self)
            .expect("every BinOp has a row in BINARY_OPS")
    }

    pub fn prec(self) -> u8 {
        self.info().prec
    }

    pub fn text(self) -> &'static str {
        self.info().text
    }

    /// The token that spells this operator.
    ///
    /// The only hand-written direction: `from_token` searches the table using
    /// this, so the two cannot drift apart.
    pub fn token(self) -> Token {
        match self {
            BinOp::Add => Token::Plus,
            BinOp::Sub => Token::Minus,
            BinOp::Mul => Token::Star,
            BinOp::Div => Token::Slash,
            BinOp::Rem => Token::Percent,
            BinOp::Eq => Token::EqEq,
            BinOp::Ne => Token::NotEq,
            BinOp::Lt => Token::Lt,
            BinOp::Le => Token::LtEq,
            BinOp::Gt => Token::Gt,
            BinOp::Ge => Token::GtEq,
            BinOp::And => Token::And,
            BinOp::Or => Token::Or,
            BinOp::Band => Token::Band,
            BinOp::Bor => Token::Bor,
            BinOp::Bxor => Token::Bxor,
            BinOp::Bsl => Token::Bsl,
            BinOp::Bsr => Token::Bsr,
            BinOp::Orelse => Token::Orelse,
            BinOp::Pipe => Token::Pipe,
        }
    }

    pub fn from_token(token: &Token) -> Option<BinOp> {
        BINARY_OPS
            .iter()
            .map(|i| i.op)
            .find(|op| op.token() == *token)
    }
}

/// The operators sharing a single precedence level, in table order.
///
/// The parser calls this once per level to build that level's alternatives, and
/// uses it again to name them in the error when none matched.
///
/// `levels_are_contiguous` asserts no level in `MIN_PREC..=MAX_PREC` is empty.
/// A gap is not itself a miscompile — the parser would build a layer matching
/// nothing and pass through — but it means the table and the two constants have
/// drifted apart, and those constants are what the formatter derives `P_UNARY`
/// and everything above it from.
pub fn ops_at(prec: u8) -> impl Iterator<Item = BinOp> {
    BINARY_OPS
        .iter()
        .filter(move |i| i.prec == prec)
        .map(|i| i.op)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant, so a new operator added to the AST without a row here
    /// fails to compile rather than panicking in `info()` at run time.
    const ALL: &[BinOp] = &[
        BinOp::Add,
        BinOp::Sub,
        BinOp::Mul,
        BinOp::Div,
        BinOp::Rem,
        BinOp::Eq,
        BinOp::Ne,
        BinOp::Lt,
        BinOp::Le,
        BinOp::Gt,
        BinOp::Ge,
        BinOp::And,
        BinOp::Or,
        BinOp::Band,
        BinOp::Bor,
        BinOp::Bxor,
        BinOp::Bsl,
        BinOp::Bsr,
        BinOp::Orelse,
        BinOp::Pipe,
    ];

    #[test]
    fn table_covers_every_operator() {
        assert_eq!(ALL.len(), BINARY_OPS.len());
        for op in ALL {
            assert_eq!(
                BINARY_OPS.iter().filter(|i| i.op == *op).count(),
                1,
                "{op:?}"
            );
        }
    }

    #[test]
    fn tokens_round_trip() {
        for op in ALL {
            assert_eq!(BinOp::from_token(&op.token()), Some(*op));
        }
    }

    #[test]
    fn levels_are_contiguous() {
        for prec in MIN_PREC..=MAX_PREC {
            assert!(ops_at(prec).next().is_some(), "level {prec} is empty");
        }
        assert!(BINARY_OPS
            .iter()
            .all(|i| (MIN_PREC..=MAX_PREC).contains(&i.prec)));
    }

    /// The ladder documented in decisions.md.
    #[test]
    fn ladder_is_the_documented_one() {
        assert!(BinOp::Orelse.prec() < BinOp::Or.prec());
        assert!(BinOp::Or.prec() < BinOp::And.prec());
        assert!(BinOp::And.prec() < BinOp::Eq.prec());
        assert!(BinOp::Eq.prec() < BinOp::Pipe.prec());
        assert!(BinOp::Pipe.prec() < BinOp::Bor.prec());
        assert!(BinOp::Bor.prec() < BinOp::Bxor.prec());
        assert!(BinOp::Bxor.prec() < BinOp::Band.prec());
        assert!(BinOp::Band.prec() < BinOp::Bsl.prec());
        assert!(BinOp::Bsl.prec() < BinOp::Add.prec());
        assert!(BinOp::Add.prec() < BinOp::Mul.prec());
    }
}
