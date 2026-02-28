use crate::error::Result;
use std::path::PathBuf;

pub fn defconfig_command(_defconfig: PathBuf, _kconfig: PathBuf, _srctree: PathBuf) -> Result<()> {
    println!("Defconfig command not yet implemented");
    Ok(())
}
