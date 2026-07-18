//! The Neon compiler: source -> C11.

pub mod ast;
pub mod diagnostic;
pub mod expand;
pub mod format;
pub mod lexer;
pub mod ops;
pub mod parser;
pub mod stdlib;
pub mod typecheck;

/// Placeholder for the pipeline entry point.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
