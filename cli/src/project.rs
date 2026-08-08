//! A Neon project: a `neon.toml` manifest with a `src/main.neon` entry point. Verbs that
//! act on a whole project (`build`, `run`) find the root by walking up from the working
//! directory until a manifest turns up, the way `cargo` does.

use color_eyre::eyre::{bail, eyre, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
struct Manifest {
    package: Package,
}

#[derive(Deserialize)]
struct Package {
    name: String,
}

/// A resolved project rooted at the directory holding its `neon.toml`.
pub struct Project {
    pub root: PathBuf,
    pub name: String,
}

impl Project {
    /// Find the project containing `start` (a file or directory), walking upward.
    pub fn find(start: &Path) -> Result<Project> {
        let mut dir = if start.is_dir() {
            Some(start)
        } else {
            start.parent()
        };
        while let Some(d) = dir {
            let manifest = d.join("neon.toml");
            if manifest.is_file() {
                return Project::load(d, &manifest);
            }
            dir = d.parent();
        }
        bail!(
            "no neon.toml found in {} or any parent directory (run `neon init` to start a project)",
            start.display()
        );
    }

    fn load(root: &Path, manifest: &Path) -> Result<Project> {
        let src = std::fs::read_to_string(manifest)?;
        let parsed: Manifest =
            toml::from_str(&src).map_err(|e| eyre!("{}: {e}", manifest.display()))?;
        Ok(Project {
            root: root.to_path_buf(),
            name: parsed.package.name,
        })
    }

    /// The application entry point.
    pub fn entry(&self) -> PathBuf {
        self.root.join("src/main.neon")
    }

    /// The build output directory, created on demand. Named `_neon` — a leading
    /// underscore sorts it aside and reads as "tooling, not source".
    pub fn target_dir(&self) -> PathBuf {
        self.root.join("_neon")
    }

    /// Where the compiled executable lands. `.exe` on Windows, nothing anywhere else --
    /// see `emit::executable_path`.
    pub fn executable(&self) -> PathBuf {
        crate::emit::executable_path(self.target_dir().join(&self.name))
    }
}
