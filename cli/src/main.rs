use color_eyre::eyre::{bail, Context, Result};
use std::path::PathBuf;

/// Locates `include/`, `lib/libneon_rt.a` and `stdlib/`.
///
/// Resolved at runtime, never baked in: a compile-time path describes the
/// machine that built the compiler, not the one running it.
pub struct Sysroot(PathBuf);

impl Sysroot {
    fn probe(dir: PathBuf) -> Option<Self> {
        dir.join("lib/libneon_rt.a").is_file().then_some(Sysroot(dir))
    }

    pub fn find() -> Result<Self> {
        if let Some(dir) = std::env::var_os("NEON_SYSROOT") {
            let dir = PathBuf::from(dir);
            return Self::probe(dir.clone()).ok_or_else(|| {
                eyre::eyre!("no lib/libneon_rt.a under '{}'", dir.display())
                    .wrap_err("NEON_SYSROOT does not point at a Neon sysroot")
            });
        }

        let exe = std::env::current_exe().wrap_err("cannot locate the neon binary")?;
        let exe_dir = exe
            .parent()
            .ok_or_else(|| eyre::eyre!("the neon binary has no parent directory"))?;

        // exe_dir: dev (target/<profile>). exe_dir/..: installed (prefix/bin).
        let candidates = [exe_dir.to_path_buf(), exe_dir.join("..")];
        for dir in &candidates {
            if let Some(found) = Self::probe(dir.clone()) {
                return Ok(found);
            }
        }

        bail!(
            "cannot find the Neon sysroot: no lib/libneon_rt.a under {}.\n\
             Set NEON_SYSROOT to override.",
            candidates
                .iter()
                .map(|p| format!("'{}'", p.display()))
                .collect::<Vec<_>>()
                .join(" or ")
        )
    }

    pub fn include(&self) -> PathBuf {
        self.0.join("include")
    }

    pub fn runtime_lib(&self) -> PathBuf {
        self.0.join("lib/libneon_rt.a")
    }

    pub fn stdlib(&self) -> PathBuf {
        self.0.join("stdlib")
    }
}

fn main() -> Result<()> {
    color_eyre::install()?;

    let sysroot = Sysroot::find().wrap_err("failed to set up the toolchain")?;
    println!("neon {}", neon_compiler::version());
    println!("sysroot: {}", sysroot.0.display());
    println!("  include: {}", sysroot.include().display());
    println!("  runtime: {}", sysroot.runtime_lib().display());
    println!("  stdlib:  {}", sysroot.stdlib().display());
    Ok(())
}
