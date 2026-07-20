//! The intermediate representation and its passes. See `docs/design/ir.md`.
//!
//! The pipeline: monomorphise → lower to SSA → optimise → insert refcounts → emit.
//! Everything here consumes what the checker already worked out (`TypecheckResult`)
//! and re-derives nothing.

pub mod effects;
pub mod lower;
pub mod opt;
pub mod refcount;
pub mod repr;
pub mod ssa;
pub mod unique;

use crate::ast::Module;
use crate::typecheck::env::Env;
use crate::typecheck::result::TypecheckResult;
use ssa::Program;

/// Which stage of the pipeline to stop at, for `neon ir`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Straight out of lowering and monomorphisation, before any pass.
    Lowered,
    /// After the optimiser.
    Optimised,
    /// After refcount insertion -- the IR that would be emitted.
    Final,
}

/// Run the IR pipeline to the requested stage: lower (with monomorphisation), then
/// optimise, then insert reference counts.
///
/// The stages are a prefix, not a menu — each early return hands back the program in a
/// genuinely intermediate state. Only `Final` is safe to emit: `Lowered` and `Optimised`
/// carry no retains or releases at all, so compiling either would leak everything and free
/// nothing. Codegen therefore always asks for `Final`, and the earlier stages exist for
/// `neon ir` to print.
///
/// The order is forced rather than chosen. The optimiser has to run before refcounting
/// because it rewrites control flow, and refcount placement is pinned to a specific CFG
/// (see `refcount::insert`); running it after would strand releases on paths that no
/// longer own the value.
pub fn compile(
    env: &Env,
    result: &TypecheckResult,
    module: &Module,
    libs: &[(Vec<String>, &Module)],
    stage: Stage,
) -> Program {
    compile_with(env, result, module, libs, stage, false)
}

/// `compile`, for `neon test`: `test` blocks are lowered as functions instead of stripped.
///
/// Everything after lowering is the same pipeline. A test body is ordinary code and gets
/// the same optimiser and the same refcount placement as any function — running tests
/// against a program built by a second, gentler pipeline would test the wrong compiler.
pub fn compile_tests(
    env: &Env,
    result: &TypecheckResult,
    module: &Module,
    libs: &[(Vec<String>, &Module)],
) -> Program {
    compile_with(env, result, module, libs, Stage::Final, true)
}

fn compile_with(
    env: &Env,
    result: &TypecheckResult,
    module: &Module,
    libs: &[(Vec<String>, &Module)],
    stage: Stage,
    tests: bool,
) -> Program {
    let mut program = lower::lower_module_with(env, result, module, libs, tests);
    if stage == Stage::Lowered {
        return program;
    }
    opt::optimize(&mut program);
    if stage == Stage::Optimised {
        return program;
    }
    refcount::insert(&mut program);
    program
}
