// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::path::PathBuf;

use crate::{
    config::{ConfigEngine, GeneratedArtifacts},
    error::Result,
    validate::validate_input_path,
};

pub fn defconfig_to_output(
    defconfig: PathBuf,
    output: PathBuf,
    kconfig: PathBuf,
    srctree: PathBuf,
) -> Result<GeneratedArtifacts> {
    for path in [&defconfig, &output, &kconfig, &srctree] {
        validate_input_path(path)?;
    }

    let mut engine = ConfigEngine::from_kconfig(&kconfig, &srctree)?;
    engine.load_config(&defconfig)?;
    // Match Linux defconfig semantics: after loading the minimal seed config,
    // recompute choice selection, visibility-gated symbols, and derived
    // defaults before emitting the expanded .config.
    engine.refresh_prompt_state();
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
