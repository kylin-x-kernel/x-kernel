// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::path::PathBuf;

use crate::{
    config::{ConfigEngine, GeneratedArtifacts},
    error::Result,
};

pub fn defconfig_to_output(
    defconfig: PathBuf,
    output: PathBuf,
    kconfig: PathBuf,
    srctree: PathBuf,
) -> Result<GeneratedArtifacts> {
    let mut engine = ConfigEngine::from_kconfig(&kconfig, &srctree)?;
    engine.load_config(&defconfig)?;
    engine.prune_inactive_symbols();
    engine.write_artifacts(output)
}

pub fn defconfig_command(defconfig: PathBuf, kconfig: PathBuf, srctree: PathBuf) -> Result<()> {
    println!("Applying defconfig...");
    println!("Defconfig: {}", defconfig.display());
    println!("Kconfig: {}", kconfig.display());

    let output = PathBuf::from(".config");
    let generated = defconfig_to_output(defconfig, output.clone(), kconfig, srctree)?;
    println!("✅ Generated {}", output.display());
    println!("✅ Generated {}", generated.auto_conf.display());
    println!("✅ Generated {}", generated.autoconf_h.display());

    Ok(())
}
