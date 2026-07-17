use crate::sysroot::Sysroot;
use color_eyre::eyre::{Context, Result};

pub fn run() -> Result<()> {
    let s = Sysroot::find().wrap_err("failed to locate the toolchain")?;
    println!("{}", s.root().display());
    println!("  include: {}", s.include().display());
    println!("  runtime: {}", s.runtime_lib().display());
    println!("  stdlib:  {}", s.stdlib().display());
    Ok(())
}
