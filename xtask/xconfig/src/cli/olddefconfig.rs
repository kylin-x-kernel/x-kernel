// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::path::PathBuf;

use crate::{cli::oldconfig::oldconfig_command, error::Result};

pub fn olddefconfig_command(config: PathBuf, kconfig: PathBuf, srctree: PathBuf) -> Result<()> {
    oldconfig_command(config, kconfig, srctree, true)
}
