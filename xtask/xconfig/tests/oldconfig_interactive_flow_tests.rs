// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{fs, io::Cursor, path::PathBuf};

use tempfile::TempDir;
use xconfig::cli::oldconfig_command_with_io;

#[test]
fn test_oldconfig_interactively_accepts_default_for_new_bool() {
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
"#,
    )
    .unwrap();
    fs::write(&config_path, "FEATURE_A=y\n").unwrap();

    let mut input = Cursor::new(b"\n".to_vec());
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
    assert!(config.contains("FEATURE_B=y"));

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Feature B (FEATURE_B)"));
}

#[test]
fn test_oldconfig_interactively_discovers_new_prompt_after_enabling_dependency() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig_path = temp_dir.path().join("Kconfig");
    let config_path = temp_dir.path().join(".config");

    fs::write(
        &kconfig_path,
        r#"
config FEATURE_A
    bool "Feature A"
    default n

config FEATURE_B
    bool "Feature B"
    depends on FEATURE_A
    default y
"#,
    )
    .unwrap();
    fs::write(&config_path, "").unwrap();

    let mut input = Cursor::new(b"y\nn\n".to_vec());
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

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Feature A (FEATURE_A)"));
    assert!(output.contains("Feature B (FEATURE_B)"));
}

#[test]
fn test_oldconfig_interactively_selects_new_choice_option() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig_path = temp_dir.path().join("Kconfig");
    let config_path = temp_dir.path().join(".config");

    fs::write(
        &kconfig_path,
        r#"
choice
    prompt "Primary option"
    default FEATURE_A

config FEATURE_A
    bool "Feature A"

config FEATURE_B
    bool "Feature B"

endchoice
"#,
    )
    .unwrap();
    fs::write(&config_path, "").unwrap();

    let mut input = Cursor::new(b"2\n".to_vec());
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
    assert!(config.contains("# FEATURE_A is not set"));
    assert!(config.contains("FEATURE_B=y"));

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Primary option:"));
    assert!(output.contains("1. Feature A (FEATURE_A)"));
    assert!(output.contains("2. Feature B (FEATURE_B)"));
}
