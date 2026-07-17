//! The type checker.
//!
//! Types are sets of values; subtyping is containment, decided as `s ∧ ¬t = ∅`.
//! See `docs/design/typechecker.md`.

pub mod bdd;
pub mod empty;
pub mod types;

#[cfg(test)]
mod tests;

pub use empty::Solver;
pub use types::{RecordAtom, TyId, Types};
