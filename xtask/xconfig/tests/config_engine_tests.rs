// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{fs, path::Path};

use tempfile::TempDir;
use xconfig::config::ConfigEngine;

fn write_kconfig(dir: &Path, content: &str) -> std::path::PathBuf {
    let path = dir.join("Kconfig");
    fs::write(&path, content).unwrap();
    path
}

#[test]
fn test_engine_builds_defaults_and_writes_artifacts() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig = write_kconfig(
        temp_dir.path(),
        r#"
config FEATURE_A
    bool "Feature A"
    default y

config GREETING
    string "Greeting"
    default "hello"
"#,
    );

    let engine = ConfigEngine::from_kconfig(&kconfig, temp_dir.path()).unwrap();
    assert_eq!(
        engine.symbols().get_value("FEATURE_A").as_deref(),
        Some("y")
    );
    assert_eq!(
        engine.symbols().get_value("GREETING").as_deref(),
        Some("hello")
    );

    let output = temp_dir.path().join(".config");
    let generated = engine.write_artifacts(&output).unwrap();

    let config = fs::read_to_string(&output).unwrap();
    assert!(config.contains("FEATURE_A=y"));
    assert!(config.contains("GREETING=\"hello\""));

    let auto_conf = fs::read_to_string(generated.auto_conf).unwrap();
    assert!(auto_conf.contains("FEATURE_A=y"));

    let autoconf_h = fs::read_to_string(generated.autoconf_h).unwrap();
    assert!(autoconf_h.contains("#define FEATURE_A 1"));
}

#[test]
fn test_engine_preserves_non_bool_defaults_on_not_set_overlay() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig = write_kconfig(
        temp_dir.path(),
        r#"
config NAME
    string "Name"
    default "fallback"
"#,
    );

    let mut engine = ConfigEngine::from_kconfig(&kconfig, temp_dir.path()).unwrap();
    let config_path = temp_dir.path().join(".config");
    fs::write(&config_path, "# NAME is not set\n").unwrap();

    engine.load_config(&config_path).unwrap();

    assert_eq!(
        engine.symbols().get_value("NAME").as_deref(),
        Some("fallback")
    );
}

#[test]
fn test_engine_prunes_inactive_symbols() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig = write_kconfig(
        temp_dir.path(),
        r#"
config PARENT
    bool "Parent"
    default n

config CHILD
    bool "Child"
    depends on PARENT
"#,
    );

    let mut engine = ConfigEngine::from_kconfig(&kconfig, temp_dir.path()).unwrap();
    let config_path = temp_dir.path().join(".config");
    fs::write(&config_path, "CHILD=y\n").unwrap();

    engine.load_config(&config_path).unwrap();
    let inactive = engine.prune_inactive_symbols();

    assert_eq!(inactive, vec!["CHILD".to_string()]);
    assert_eq!(engine.symbols().get_value("CHILD"), None);
}

#[test]
fn test_engine_applies_choice_default() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig = write_kconfig(
        temp_dir.path(),
        r#"
choice
    prompt "Primary option"
    default FEATURE_B

config FEATURE_A
    bool "Feature A"

config FEATURE_B
    bool "Feature B"
endchoice
"#,
    );

    let engine = ConfigEngine::from_kconfig(&kconfig, temp_dir.path()).unwrap();
    assert_eq!(
        engine.symbols().get_value("FEATURE_B").as_deref(),
        Some("y")
    );
    assert_eq!(engine.symbols().get_value("FEATURE_A"), None);
}
