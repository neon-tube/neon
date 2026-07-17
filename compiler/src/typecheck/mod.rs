//! The type checker.
//!
//! Types are sets of values; subtyping is containment, decided as `s ∧ ¬t = ∅`.
//! See `docs/design/typechecker.md`.

pub mod bdd;
pub mod dispatch;
pub mod empty;
pub mod env;
pub mod narrow;
pub mod print;
pub mod resolve;
pub mod types;

#[cfg(test)]
mod dispatch_tests;
#[cfg(test)]
mod env_tests;
#[cfg(test)]
mod tests;

pub use empty::Solver;
pub use env::{Env, TypeError, TypeErrorKind};
pub use resolve::Scope;
pub use types::{RecordAtom, TyId, Types};
