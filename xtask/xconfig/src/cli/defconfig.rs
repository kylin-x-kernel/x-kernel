use std::path::PathBuf;

use crate::error::Result;

pub fn defconfig_command(_defconfig: PathBuf, _kconfig: PathBuf, _srctree: PathBuf) -> Result<()> {
    println!("Defconfig command not yet implemented");
    Ok(())
}
