use crate::buildcfg::{BuildConfig, BuildFlags};
use crate::{emit, frontend, project::Project};
use color_eyre::eyre::{eyre, Result};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Build and run. With a `.neon` file, compiles just that file; otherwise builds the
/// project containing `path` (or the working directory). Trailing `args` reach the program.
pub fn run(path: Option<OsString>, args: Vec<OsString>, flags: BuildFlags) -> Result<()> {
    let exe = match path {
        Some(p) if Path::new(&p).extension().is_some_and(|e| e == "neon") => {
            build_file(&PathBuf::from(p), flags)?
        }
        Some(p) => crate::cmd::build::build(&Project::find(&PathBuf::from(p))?, flags)?,
        None => {
            let cwd = std::env::current_dir()?;
            crate::cmd::build::build(&Project::find(&cwd)?, flags)?
        }
    };
    exec(&exe, &args)
}

/// Compile a lone file into a temporary executable and return its path.
fn build_file(path: &Path, flags: BuildFlags) -> Result<PathBuf> {
    let checked = frontend::check(path, false)?;
    let cfg = BuildConfig::resolve(path, flags)?;
    let dir = std::env::temp_dir().join("neon-run");
    std::fs::create_dir_all(&dir)?;
    let stem = path.file_stem().unwrap_or_else(|| "program".as_ref());
    let out = emit::executable_path(dir.join(stem));
    emit::to_executable(&checked, &out, &cfg)?;
    Ok(out)
}

/// Replace this process's exit status with the program's, forwarding its arguments.
fn exec(exe: &Path, args: &[OsString]) -> Result<()> {
    let status = Command::new(exe)
        .args(args)
        .status()
        .map_err(|e| eyre!("could not run {}: {e}", exe.display()))?;
    std::process::exit(status.code().unwrap_or(1));
}
