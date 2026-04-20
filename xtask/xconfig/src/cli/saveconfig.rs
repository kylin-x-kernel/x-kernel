// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::path::PathBuf;

use crate::{config::ConfigEngine, error::Result};

pub fn saveconfig_command(output: PathBuf, kconfig: PathBuf, srctree: PathBuf) -> Result<()> {
    println!("Saving configuration...");
    println!("Kconfig: {}", kconfig.display());
    println!("Output: {}", output.display());

    let mut engine = ConfigEngine::from_kconfig(&kconfig, &srctree)?;
    engine.prune_inactive_symbols();
    let generated = engine.write_artifacts(&output)?;
    println!("✅ Saved .config to {}", output.display());
    println!("✅ Generated {}", generated.auto_conf.display());
    println!("✅ Generated {}", generated.autoconf_h.display());

    Ok(())
}
