//! The type checker.
//!
//! Types are sets of values; subtyping is containment, decided as `s ∧ ¬t = ∅`.
//! See `docs/design/typechecker.md`.

pub mod bdd;
pub mod check;
pub mod dispatch;
pub mod empty;
pub mod generic;
pub mod env;
pub mod narrow;
pub mod ordered;
pub mod print;
pub mod resolve;
pub mod result;
pub mod types;

#[cfg(test)]
mod check_tests;
#[cfg(test)]
mod dispatch_tests;
#[cfg(test)]
mod env_tests;
#[cfg(test)]
mod generic_tests;
#[cfg(test)]
mod tests;

pub use empty::Solver;
pub use env::{Env, TypeError, TypeErrorKind};
pub use resolve::Scope;
pub use types::{RecordAtom, TyId, Types};

/// The constructor head of a nominal type, for the env's bound check.
pub(crate) fn nominal_head_of(env: &Env, ty: TyId) -> Option<String> {
    dispatch::nominal_head(env, ty)
}
