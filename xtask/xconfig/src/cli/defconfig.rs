// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::path::PathBuf;

use crate::error::Result;

pub fn defconfig_command(_defconfig: PathBuf, _kconfig: PathBuf, _srctree: PathBuf) -> Result<()> {
    println!("Defconfig command not yet implemented");
    Ok(())
}
