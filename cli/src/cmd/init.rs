use color_eyre::eyre::{bail, Result};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

const MAIN_NEON: &str = "use std::io\n\nfn main() {\n    io::println(\"Hello, Neon!\")\n}\n";

/// Scaffold a project. With a name, creates a directory of that name; without one, uses the
/// working directory. Writes a `neon.toml` and a `src/main.neon`, never overwriting.
pub fn run(name: Option<OsString>) -> Result<()> {
    let (root, project_name) = match name {
        Some(n) => {
            let root = PathBuf::from(&n);
            let name = root
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| n.to_string_lossy().into_owned());
            (root, name)
        }
        None => {
            let cwd = std::env::current_dir()?;
            let name = cwd
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "app".to_string());
            (cwd, name)
        }
    };

    let manifest = root.join("neon.toml");
    if manifest.exists() {
        bail!("{} already exists", manifest.display());
    }

    std::fs::create_dir_all(root.join("src"))?;
    write_new(
        &manifest,
        &format!("[package]\nname = \"{project_name}\"\nversion = \"0.1.0\"\n"),
    )?;
    write_new(&root.join("src/main.neon"), MAIN_NEON)?;

    println!("created project `{project_name}` in {}", root.display());
    Ok(())
}

/// Write a file only if it does not already exist.
fn write_new(path: &Path, contents: &str) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    std::fs::write(path, contents)?;
    Ok(())
}
