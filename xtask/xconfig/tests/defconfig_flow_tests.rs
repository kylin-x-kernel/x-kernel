// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::fs;

use tempfile::TempDir;
use xconfig::cli::defconfig_to_output;

#[test]
fn test_defconfig_expands_minimal_input_into_full_config() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig_path = temp_dir.path().join("Kconfig");
    let defconfig_path = temp_dir.path().join("mini_defconfig");
    let output_path = temp_dir.path().join(".config");

    fs::write(
        &kconfig_path,
        r#"
config FEATURE_A
    bool "Feature A"
    default y

config FEATURE_B
    bool "Feature B"
    default n

config NAME
    string "Name"
    default "fallback"
"#,
    )
    .unwrap();
    fs::write(&defconfig_path, "# FEATURE_A is not set\nFEATURE_B=y\n").unwrap();

    let generated = defconfig_to_output(
        defconfig_path,
        output_path.clone(),
        kconfig_path,
        temp_dir.path().to_path_buf(),
    )
    .unwrap();

    let config = fs::read_to_string(output_path).unwrap();
    assert!(config.contains("# FEATURE_A is not set"));
    assert!(config.contains("FEATURE_B=y"));
    assert!(config.contains("NAME=\"fallback\""));

    let auto_conf = fs::read_to_string(generated.auto_conf).unwrap();
    assert!(auto_conf.contains("FEATURE_B=y"));
    assert!(!auto_conf.contains("FEATURE_A=y"));
}
