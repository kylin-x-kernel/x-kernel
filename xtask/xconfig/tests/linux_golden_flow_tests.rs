// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{fs, path::PathBuf};

use tempfile::TempDir;
use xconfig::cli::{defconfig_to_output, olddefconfig_command, saveconfig_command, savedefconfig_command};

fn config_body(content: &str) -> String {
    content
        .lines()
        .skip(4)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

#[test]
fn test_defconfig_golden_output_uses_effective_defaults() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig_path = temp_dir.path().join("Kconfig");
    let output_path = temp_dir.path().join(".config");
    let defconfig_path = temp_dir.path().join("defconfig");

    fs::write(
        &kconfig_path,
        r#"
config FEATURE_A
    bool "Feature A"
    default y

config FEATURE_B
    bool "Feature B"
    depends on FEATURE_A
    default n

config NAME
    string "Name"
    default "fallback"
"#,
    )
    .unwrap();
    fs::write(&defconfig_path, "FEATURE_B=y\n").unwrap();

    defconfig_to_output(
        defconfig_path,
        output_path.clone(),
        kconfig_path,
        PathBuf::from(temp_dir.path()),
    )
    .unwrap();

    let actual = config_body(&fs::read_to_string(output_path).unwrap());
    let expected = r#"FEATURE_A=y
FEATURE_B=y
NAME="fallback""#;
    assert_eq!(actual, expected);
}

#[test]
fn test_defconfig_golden_output_applies_selects_and_implies() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig_path = temp_dir.path().join("Kconfig");
    let output_path = temp_dir.path().join(".config");
    let defconfig_path = temp_dir.path().join("defconfig");

    fs::write(
        &kconfig_path,
        r#"
config HELPER_SELECTED
    bool "Selected helper"

config HELPER_IMPLIED
    bool "Implied helper"

config HELPER_EXPLICIT
    bool "Explicit helper"

config SELECTOR
    bool "Selector"
    select HELPER_SELECTED
    imply HELPER_IMPLIED
    imply HELPER_EXPLICIT
"#,
    )
    .unwrap();
    fs::write(
        &defconfig_path,
        "SELECTOR=y\n# HELPER_EXPLICIT is not set\n",
    )
    .unwrap();

    defconfig_to_output(
        defconfig_path,
        output_path.clone(),
        kconfig_path,
        PathBuf::from(temp_dir.path()),
    )
    .unwrap();

    let actual = config_body(&fs::read_to_string(output_path).unwrap());
    let expected = r#"# HELPER_EXPLICIT is not set
HELPER_IMPLIED=y
HELPER_SELECTED=y
SELECTOR=y"#;
    assert_eq!(actual, expected);
}

#[test]
fn test_defconfig_golden_output_applies_transitive_select_imply_chain() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig_path = temp_dir.path().join("Kconfig");
    let output_path = temp_dir.path().join(".config");
    let defconfig_path = temp_dir.path().join("defconfig");

    fs::write(
        &kconfig_path,
        r#"
config IMPLIED
    bool "Implied"

config SELECTED
    bool "Selected"
    imply IMPLIED

config ROOT
    bool "Root"
    select SELECTED
"#,
    )
    .unwrap();
    fs::write(&defconfig_path, "ROOT=y\n").unwrap();

    defconfig_to_output(
        defconfig_path,
        output_path.clone(),
        kconfig_path,
        PathBuf::from(temp_dir.path()),
    )
    .unwrap();

    let actual = config_body(&fs::read_to_string(output_path).unwrap());
    let expected = r#"IMPLIED=y
ROOT=y
SELECTED=y"#;
    assert_eq!(actual, expected);
}

#[test]
fn test_olddefconfig_golden_output_applies_defaults_to_new_symbols() {
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

    let actual = config_body(&fs::read_to_string(config_path).unwrap());
    let expected = r#"FEATURE_A=y
FEATURE_B=y
NAME="fallback""#;
    assert_eq!(actual, expected);
}

#[test]
fn test_olddefconfig_golden_output_applies_selects() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig_path = temp_dir.path().join("Kconfig");
    let config_path = temp_dir.path().join(".config");

    fs::write(
        &kconfig_path,
        r#"
config HELPER
    bool "Helper"

config SELECTOR
    bool "Selector"
    select HELPER
"#,
    )
    .unwrap();
    fs::write(&config_path, "SELECTOR=y\n").unwrap();

    olddefconfig_command(
        config_path.clone(),
        kconfig_path,
        PathBuf::from(temp_dir.path()),
    )
    .unwrap();

    let actual = config_body(&fs::read_to_string(config_path).unwrap());
    let expected = r#"HELPER=y
SELECTOR=y"#;
    assert_eq!(actual, expected);
}

#[test]
fn test_defconfig_golden_output_clamps_range_violations() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig_path = temp_dir.path().join("Kconfig");
    let output_path = temp_dir.path().join(".config");
    let defconfig_path = temp_dir.path().join("defconfig");

    fs::write(
        &kconfig_path,
        r#"
config LIMIT_MIN
    u32
    default 4

config LIMIT_MAX
    u32
    default 8

config COUNT
    u32 "Count"
    range LIMIT_MIN LIMIT_MAX
    default 6
"#,
    )
    .unwrap();
    fs::write(&defconfig_path, "COUNT=99\n").unwrap();

    defconfig_to_output(
        defconfig_path,
        output_path.clone(),
        kconfig_path,
        PathBuf::from(temp_dir.path()),
    )
    .unwrap();

    let actual = config_body(&fs::read_to_string(output_path).unwrap());
    let expected = r#"COUNT=8
LIMIT_MAX=8
LIMIT_MIN=4"#;
    assert_eq!(actual, expected);
}

#[test]
fn test_saveconfig_golden_output_preserves_effective_config() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig_path = temp_dir.path().join("Kconfig");
    let config_path = temp_dir.path().join(".config");

    fs::write(
        &kconfig_path,
        r#"
config HELPER
    bool "Helper"

config SELECTOR
    bool "Selector"
    select HELPER

config LIMIT_MIN
    u32
    default 4

config LIMIT_MAX
    u32
    default 8

config COUNT
    u32 "Count"
    range LIMIT_MIN LIMIT_MAX
    default 6
"#,
    )
    .unwrap();
    fs::write(&config_path, "SELECTOR=y\nCOUNT=99\n").unwrap();

    saveconfig_command(
        config_path.clone(),
        kconfig_path,
        PathBuf::from(temp_dir.path()),
    )
    .unwrap();

    let actual = config_body(&fs::read_to_string(config_path).unwrap());
    let expected = r#"COUNT=8
HELPER=y
LIMIT_MAX=8
LIMIT_MIN=4
SELECTOR=y"#;
    assert_eq!(actual, expected);
}

#[test]
fn test_saveconfig_golden_output_preserves_transitive_select_imply_chain() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig_path = temp_dir.path().join("Kconfig");
    let config_path = temp_dir.path().join(".config");

    fs::write(
        &kconfig_path,
        r#"
config IMPLIED
    bool "Implied"

config SELECTED
    bool "Selected"
    imply IMPLIED

config ROOT
    bool "Root"
    select SELECTED
"#,
    )
    .unwrap();
    fs::write(&config_path, "ROOT=y\n").unwrap();

    saveconfig_command(
        config_path.clone(),
        kconfig_path,
        PathBuf::from(temp_dir.path()),
    )
    .unwrap();

    let actual = config_body(&fs::read_to_string(config_path).unwrap());
    let expected = r#"IMPLIED=y
ROOT=y
SELECTED=y"#;
    assert_eq!(actual, expected);
}

#[test]
fn test_savedefconfig_golden_output_keeps_only_minimal_prompted_values() {
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

config MODE_NAME
    string "Mode name"
    default "enabled" if FEATURE_A
    default "disabled"

config INTERNAL_MODE
    string
    default "auto"
"#,
    )
    .unwrap();
    fs::write(
        &config_path,
        "# FEATURE_A is not set\nFEATURE_B=y\nMODE_NAME=\"disabled\"\nINTERNAL_MODE=\"manual\"\n",
    )
    .unwrap();

    savedefconfig_command(
        config_path,
        output_path.clone(),
        kconfig_path,
        PathBuf::from(temp_dir.path()),
    )
    .unwrap();

    let actual = config_body(&fs::read_to_string(output_path).unwrap());
    let expected = r#"# FEATURE_A is not set
FEATURE_B=y"#;
    assert_eq!(actual, expected);
}
