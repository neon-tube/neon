//! The intermediate representation and its passes. See `docs/design/ir.md`.
//!
//! The pipeline: monomorphise → lower to SSA → optimise → insert refcounts → emit.
//! Everything here consumes what the checker already worked out (`TypecheckResult`)
//! and re-derives nothing.

pub mod repr;
