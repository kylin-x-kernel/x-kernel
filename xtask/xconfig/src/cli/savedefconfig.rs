// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::path::PathBuf;

use crate::{
    config::{ConfigEngine, ConfigWriter},
    error::Result,
};

pub fn savedefconfig_command(
    config: PathBuf,
    output: PathBuf,
    kconfig: PathBuf,
    srctree: PathBuf,
) -> Result<()> {
    println!("Saving minimal defconfig...");
    println!("Config: {}", config.display());
    println!("Output: {}", output.display());

    let mut current = ConfigEngine::from_kconfig(&kconfig, &srctree)?;
    current.load_config(&config)?;
    current.refresh_prompt_state();

    let minimal = current.minimal_symbols_against_defaults();
    ConfigWriter::write(&output, &minimal)?;

    println!("✅ Saved minimal defconfig to {}", output.display());
    Ok(())
}
