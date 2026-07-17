use color_eyre::eyre::{Context, Result};
use std::path::Path;

pub fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).wrap_err_with(|| format!("cannot read '{}'", path.display()))
}
