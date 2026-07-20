//! Kani proof harnesses for the compiler.
//!
//! Everything here is behind `#[cfg(kani)]`, so this crate is empty unless cargo-kani is
//! the one building it. `verify/README.md` explains what is proved and why these
//! particular functions are the ones worth a model checker.

#[cfg(kani)]
mod distinct;
#[cfg(kani)]
mod fold;
