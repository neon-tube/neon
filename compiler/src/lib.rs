//! The Neon compiler: source -> C11.
//!
//! The pipeline runs front to back in the order the modules are listed below:
//! `lexer` turns bytes into tokens plus a side table of trivia; `parser` turns
//! tokens into `ast`; `expand` and `stdlib` supply and rewrite declarations;
//! `typecheck` resolves and checks them; `ir` lowers to SSA, chooses
//! representations and inserts reference counting; `backend` emits C11.
//! `diagnostic` renders errors from any stage, and `format` is the one consumer
//! that runs the front half for its own sake rather than to compile.
//!
//! `ops` sits outside that chain on purpose: it is the single operator
//! precedence table, and both `parser` and `format` read it. Neither keeps a
//! copy, because when they did the formatter reprinted `1 - (2 - 3)` as
//! `1 - 2 - 3` — a silent change of meaning dressed up as a cosmetic one.

pub mod ast;
pub mod backend;
pub mod derive;
pub mod diagnostic;
pub mod expand;
pub mod format;
pub mod ir;
pub mod lexer;
pub mod ops;
pub mod parser;
pub mod stdlib;
pub mod typecheck;

/// The crate version, baked in at compile time. The CLI's internal-compiler-error
/// hook prints it, so a bug report says which compiler panicked.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
