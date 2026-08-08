use crate::frontend;
use color_eyre::eyre::Result;
use neon_compiler::ir::{self, ssa::print, Stage};
use std::ffi::OsString;
use std::path::PathBuf;

/// Emit the IR for a source file at the requested pipeline stage.
pub fn run(file: &OsString, stage: Stage) -> Result<()> {
    let checked = frontend::check(&PathBuf::from(file), false, &[])?;
    let libs: Vec<(Vec<String>, &_)> = checked.libs.iter().map(|(p, m)| (p.clone(), m)).collect();
    let program = ir::compile(
        &checked.env,
        &checked.result,
        &checked.module,
        &libs,
        stage,
        Some(&checked.source),
    );
    print!("{}", print::program(&program));
    Ok(())
}
