// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::path::PathBuf;

use crate::{config::ConfigEngine, error::Result, validate::validate_input_path};

pub fn saveconfig_command(output: PathBuf, kconfig: PathBuf, srctree: PathBuf) -> Result<()> {
    for path in [&output, &kconfig, &srctree] {
        validate_input_path(path)?;
    }

    println!("Saving configuration...");
    println!("Kconfig: {}", kconfig.display());
    println!("Output: {}", output.display());

    let mut engine = ConfigEngine::from_kconfig(&kconfig, &srctree)?;
    engine.load_config(&output)?;
    engine.refresh_prompt_state();
    engine.prune_inactive_symbols();
    let generated = engine.write_artifacts(&output)?;
    println!("✅ Saved .config to {}", output.display());
    println!("✅ Generated {}", generated.auto_conf.display());
    println!("✅ Generated {}", generated.autoconf_h.display());

    Ok(())
}
