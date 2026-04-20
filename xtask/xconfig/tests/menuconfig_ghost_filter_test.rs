// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Tests for ghost config filtering in menuconfig
//!
//! Verifies that configs with unsatisfied dependencies are cleared when loading .config

use std::fs;

use tempfile::TempDir;
use xconfig::{config::ConfigEngine, kconfig::SymbolTable};

/// Helper to simulate menuconfig loading behavior via ConfigEngine
fn simulate_menuconfig_load(
    kconfig_path: &std::path::Path,
    srctree: &std::path::Path,
    config_path: &std::path::Path,
) -> SymbolTable {
    let mut engine = ConfigEngine::from_kconfig(kconfig_path, srctree).unwrap();
    if config_path.exists() {
        engine.load_menuconfig_config(config_path).unwrap();
    }
    engine.into_symbols()
}

#[test]
fn test_ghost_config_filtered_on_load() {
    let temp_dir = TempDir::new().unwrap();

    // Create Kconfig with architecture-specific config
    let kconfig_content = r#"
config ARCH_X86_64
    bool "x86_64 architecture"

config ARCH_AARCH64
    bool "aarch64 architecture"

config PMU_ENABLE
    bool "Enable PMU"
    depends on ARCH_AARCH64
    help
      Enable Performance Monitoring Unit

config UART_ENABLE
    bool "Enable UART"
    depends on ARCH_AARCH64
    help
      Enable UART device
"#;
    let kconfig_path = temp_dir.path().join("Kconfig");
    fs::write(&kconfig_path, kconfig_content).unwrap();

    // Create .config with x86_64 selected but aarch64-specific configs present (ghost configs)
    let config_content = r#"
ARCH_X86_64=y
# ARCH_AARCH64 is not set
PMU_ENABLE=y
UART_ENABLE=y
"#;
    let config_path = temp_dir.path().join(".config");
    fs::write(&config_path, config_content).unwrap();

    // Simulate menuconfig load
    let symbol_table = simulate_menuconfig_load(&kconfig_path, temp_dir.path(), &config_path);

    // Verify: x86_64 should be preserved
    assert_eq!(
        symbol_table.get_value("ARCH_X86_64"),
        Some("y".to_string()),
        "ARCH_X86_64 should be preserved"
    );

    // Verify: Ghost configs should be cleared
    assert_eq!(
        symbol_table.get_value("PMU_ENABLE"),
        None,
        "PMU_ENABLE should be cleared (depends on ARCH_AARCH64 which is not set)"
    );

    assert_eq!(
        symbol_table.get_value("UART_ENABLE"),
        None,
        "UART_ENABLE should be cleared (depends on ARCH_AARCH64 which is not set)"
    );
}

#[test]
fn test_valid_config_preserved() {
    let temp_dir = TempDir::new().unwrap();

    let kconfig_content = r#"
config ARCH_AARCH64
    bool "aarch64 architecture"

config PMU_ENABLE
    bool "Enable PMU"
    depends on ARCH_AARCH64
"#;
    let kconfig_path = temp_dir.path().join("Kconfig");
    fs::write(&kconfig_path, kconfig_content).unwrap();

    // Config with dependencies satisfied
    let config_content = r#"
ARCH_AARCH64=y
PMU_ENABLE=y
"#;
    let config_path = temp_dir.path().join(".config");
    fs::write(&config_path, config_content).unwrap();

    let symbol_table = simulate_menuconfig_load(&kconfig_path, temp_dir.path(), &config_path);

    // Verify: Both configs should be preserved
    assert_eq!(
        symbol_table.get_value("ARCH_AARCH64"),
        Some("y".to_string())
    );
    assert_eq!(
        symbol_table.get_value("PMU_ENABLE"),
        Some("y".to_string()),
        "PMU_ENABLE should be preserved when ARCH_AARCH64=y"
    );
}

#[test]
fn test_complex_dependency_chain() {
    let temp_dir = TempDir::new().unwrap();

    let kconfig_content = r#"
config FEATURE_A
    bool "Feature A"

config FEATURE_B
    bool "Feature B"
    depends on FEATURE_A

config FEATURE_C
    bool "Feature C"
    depends on FEATURE_B
"#;
    let kconfig_path = temp_dir.path().join("Kconfig");
    fs::write(&kconfig_path, kconfig_content).unwrap();

    // Config with broken dependency chain
    let config_content = r#"
# FEATURE_A is not set
# FEATURE_B is not set
FEATURE_C=y
"#;
    let config_path = temp_dir.path().join(".config");
    fs::write(&config_path, config_content).unwrap();

    let symbol_table = simulate_menuconfig_load(&kconfig_path, temp_dir.path(), &config_path);

    // FEATURE_C should be cleared because FEATURE_B (its dependency) is not satisfied
    assert_eq!(
        symbol_table.get_value("FEATURE_C"),
        None,
        "FEATURE_C should be cleared when dependency chain is broken"
    );
}
