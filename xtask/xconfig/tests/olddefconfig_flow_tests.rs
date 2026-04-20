// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{fs, io::Cursor, path::PathBuf};

use tempfile::TempDir;
use xconfig::cli::{oldconfig_command, oldconfig_command_with_io, olddefconfig_command};

#[test]
fn test_oldconfig_auto_defaults_preserves_kconfig_defaults_for_new_symbols() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig_path = temp_dir.path().join("Kconfig");
    let config_path = temp_dir.path().join(".config");

    fs::write(
        &kconfig_path,
        r#"
config FEATURE_A
    bool "Feature A"
    default y

config FEATURE_B
    bool "Feature B"
    default y

config NAME
    string "Name"
    default "fallback"
"#,
    )
    .unwrap();
    fs::write(&config_path, "FEATURE_A=y\n").unwrap();

    oldconfig_command(
        config_path.clone(),
        kconfig_path,
        PathBuf::from(temp_dir.path()),
        true,
    )
    .unwrap();

    let config = fs::read_to_string(config_path).unwrap();
    assert!(config.contains("FEATURE_A=y"));
    assert!(config.contains("FEATURE_B=y"));
    assert!(config.contains("NAME=\"fallback\""));
}

#[test]
fn test_oldconfig_without_auto_defaults_prompts_for_new_symbols() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig_path = temp_dir.path().join("Kconfig");
    let config_path = temp_dir.path().join(".config");

    fs::write(
        &kconfig_path,
        r#"
config FEATURE_A
    bool "Feature A"
    default y

config FEATURE_B
    bool "Feature B"
    default y

config NAME
    string "Name"
    default "fallback"
"#,
    )
    .unwrap();
    fs::write(&config_path, "FEATURE_A=y\n").unwrap();

    let mut input = Cursor::new(b"n\n\n".to_vec());
    let mut output = Vec::new();

    oldconfig_command_with_io(
        config_path.clone(),
        kconfig_path,
        PathBuf::from(temp_dir.path()),
        false,
        &mut input,
        &mut output,
    )
    .unwrap();

    let config = fs::read_to_string(config_path).unwrap();
    assert!(config.contains("FEATURE_A=y"));
    assert!(config.contains("# FEATURE_B is not set"));
    assert!(config.contains("NAME=\"fallback\""));
}

#[test]
fn test_olddefconfig_applies_defaults_to_new_symbols() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig_path = temp_dir.path().join("Kconfig");
    let config_path = temp_dir.path().join(".config");

    fs::write(
        &kconfig_path,
        r#"
config FEATURE_A
    bool "Feature A"
    default y

config FEATURE_B
    bool "Feature B"
    default y

config NAME
    string "Name"
    default "fallback"
"#,
    )
    .unwrap();
    fs::write(&config_path, "FEATURE_A=y\n").unwrap();

    olddefconfig_command(
        config_path.clone(),
        kconfig_path,
        PathBuf::from(temp_dir.path()),
    )
    .unwrap();

    let config = fs::read_to_string(config_path).unwrap();
    assert!(config.contains("FEATURE_A=y"));
    assert!(config.contains("FEATURE_B=y"));
    assert!(config.contains("NAME=\"fallback\""));
}
