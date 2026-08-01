use crate::buildcfg::{BuildConfig, BuildFlags};
use crate::{emit, frontend};
use color_eyre::eyre::Result;
use std::ffi::OsString;
use std::path::PathBuf;

/// Compile a single source file to an executable. The output defaults to the source name
/// without its extension.
pub fn run(file: &OsString, output: Option<OsString>, flags: BuildFlags) -> Result<()> {
    let path = PathBuf::from(file);
    let checked = frontend::check(&path, false)?;
    let cfg = BuildConfig::resolve(&path, flags)?;
    let out = emit::executable_path(
        output.map(PathBuf::from).unwrap_or_else(|| path.with_extension("")),
    );
    emit::to_executable(&checked, &out, &cfg)
}
