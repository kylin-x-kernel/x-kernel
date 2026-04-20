// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{fs, path::PathBuf};

use tempfile::TempDir;
use xconfig::cli::savedefconfig_command;

#[test]
fn test_savedefconfig_writes_only_non_default_values() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig_path = temp_dir.path().join("Kconfig");
    let config_path = temp_dir.path().join(".config");
    let output_path = temp_dir.path().join("defconfig");

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

config COUNT
    u32 "Count"
    default 4
"#,
    )
    .unwrap();
    fs::write(
        &config_path,
        "FEATURE_B=y\n# FEATURE_A is not set\nNAME=\"fallback\"\nCOUNT=4\n",
    )
    .unwrap();

    savedefconfig_command(
        config_path,
        output_path.clone(),
        kconfig_path,
        PathBuf::from(temp_dir.path()),
    )
    .unwrap();

    let saved = fs::read_to_string(output_path).unwrap();
    assert!(saved.contains("FEATURE_B=y"));
    assert!(saved.contains("# FEATURE_A is not set"));
    assert!(!saved.contains("NAME=\"fallback\""));
    assert!(!saved.contains("COUNT=4"));
}

#[test]
fn test_savedefconfig_replays_conditional_defaults_before_minimizing() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig_path = temp_dir.path().join("Kconfig");
    let config_path = temp_dir.path().join(".config");
    let output_path = temp_dir.path().join("defconfig");

    fs::write(
        &kconfig_path,
        r#"
config FEATURE_A
    bool "Feature A"
    default y

config MODE_NAME
    string "Mode name"
    default "enabled" if FEATURE_A
    default "disabled"
"#,
    )
    .unwrap();
    fs::write(&config_path, "# FEATURE_A is not set\n").unwrap();

    savedefconfig_command(
        config_path,
        output_path.clone(),
        kconfig_path,
        PathBuf::from(temp_dir.path()),
    )
    .unwrap();

    let saved = fs::read_to_string(output_path).unwrap();
    assert!(saved.contains("# FEATURE_A is not set"));
    assert!(!saved.contains("MODE_NAME="));
}

#[test]
fn test_savedefconfig_omits_promptless_symbols_even_if_present_in_config() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig_path = temp_dir.path().join("Kconfig");
    let config_path = temp_dir.path().join(".config");
    let output_path = temp_dir.path().join("defconfig");

    fs::write(
        &kconfig_path,
        r#"
config FEATURE_A
    bool "Feature A"
    default n

config INTERNAL_MODE
    string
    default "auto"
"#,
    )
    .unwrap();
    fs::write(&config_path, "FEATURE_A=y\nINTERNAL_MODE=\"manual\"\n").unwrap();

    savedefconfig_command(
        config_path,
        output_path.clone(),
        kconfig_path,
        PathBuf::from(temp_dir.path()),
    )
    .unwrap();

    let saved = fs::read_to_string(output_path).unwrap();
    assert!(saved.contains("FEATURE_A=y"));
    assert!(!saved.contains("INTERNAL_MODE="));
}
