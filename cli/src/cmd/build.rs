use crate::buildcfg::{BuildConfig, BuildFlags};
use crate::{emit, frontend, project::Project};
use color_eyre::eyre::{eyre, Result};
use std::path::PathBuf;

/// Build the project containing the working directory into `target/<name>`.
pub fn run(flags: BuildFlags) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let project = Project::find(&cwd)?;
    build(&project, flags)?;
    Ok(())
}

/// Compile a project — the entry plus every other `src/**/*.neon` as a module — into
/// its target executable, returning the path.
pub fn build(project: &Project, flags: BuildFlags) -> Result<PathBuf> {
    let entry = project.entry();
    if !entry.is_file() {
        return Err(eyre!("missing entry point {}", entry.display()));
    }
    // The build config first: it carries the `@cfg` keys the front end must expand under.
    let cfg = BuildConfig::resolve(&project.root, flags)?;
    let modules = project.modules()?;
    let checked = frontend::check_project(&entry, &modules, false, &cfg.cfg)?;
    std::fs::create_dir_all(project.target_dir())?;
    let out = project.executable();
    emit::to_executable(&checked, &out, &cfg)?;
    Ok(out)
}
