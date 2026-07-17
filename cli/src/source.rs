use color_eyre::eyre::{Context, Result};
use std::path::Path;

pub fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).wrap_err_with(|| format!("cannot read '{}'", path.display()))
}

/// 1-based line number for a byte offset. Placeholder until diagnostics land.
pub fn line_of(src: &str, offset: usize) -> usize {
    src[..offset.min(src.len())].bytes().filter(|b| *b == b'\n').count() + 1
}
