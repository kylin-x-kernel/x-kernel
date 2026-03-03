// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::path::PathBuf;

use tempfile::TempDir;
use xconfig::{
    config::{ConfigGenerator, ConfigReader, ConfigWriter, OldConfigLoader},
    kconfig::{Parser, SymbolTable, SymbolType},
};

#[test]
fn test_complete_workflow() {
    let temp_dir = TempDir::new().unwrap();

    // 1. Parse Kconfig file
    let kconfig_path = PathBuf::from("tests/fixtures/basic/Kconfig");
    let srctree = PathBuf::from("tests/fixtures/basic");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let ast = parser.parse().unwrap();

    assert_eq!(ast.entries.len(), 3);

    // 2. Build symbol table
    let mut symbols = SymbolTable::new();
    symbols.add_symbol("TEST_BOOL".to_string(), SymbolType::Bool);
    symbols.set_value("TEST_BOOL", "y".to_string());
    symbols.add_symbol("TEST_STRING".to_string(), SymbolType::String);
    symbols.set_value("TEST_STRING", "hello".to_string());
    symbols.add_symbol("TEST_INT".to_string(), SymbolType::U32);
    symbols.set_value("TEST_INT", "42".to_string());

    // 3. Write .config
    let config_path = temp_dir.path().join(".config");
    ConfigWriter::write(&config_path, &symbols).unwrap();

    // 4. Read .config back
    let config = ConfigReader::read(&config_path).unwrap();
    assert_eq!(config.get("TEST_BOOL"), Some(&"y".to_string()));
    assert_eq!(config.get("TEST_STRING"), Some(&"hello".to_string()));
    assert_eq!(config.get("TEST_INT"), Some(&"42".to_string()));

    // 5. Generate auto.conf
    let auto_conf_path = temp_dir.path().join("auto.conf");
    ConfigGenerator::generate_auto_conf(&auto_conf_path, &symbols).unwrap();

    let auto_conf = std::fs::read_to_string(&auto_conf_path).unwrap();
    assert!(auto_conf.contains("TEST_BOOL=y"));
    assert!(auto_conf.contains("TEST_STRING=hello"));
    assert!(auto_conf.contains("TEST_INT=42"));
    // Should NOT contain CONFIG_ prefix
    assert!(!auto_conf.contains("CONFIG_"));

    // 6. Generate autoconf.h
    let autoconf_h_path = temp_dir.path().join("autoconf.h");
    ConfigGenerator::generate_autoconf_h(&autoconf_h_path, &symbols).unwrap();

    let autoconf_h = std::fs::read_to_string(&autoconf_h_path).unwrap();
    assert!(autoconf_h.contains("#define TEST_BOOL 1"));
    assert!(autoconf_h.contains("#define TEST_STRING \"hello\""));
    assert!(autoconf_h.contains("#define TEST_INT \"42\""));
    // Should NOT contain CONFIG_ prefix
    assert!(!autoconf_h.contains("CONFIG_"));
}

#[test]
fn test_source_recursion_workflow() {
    // Parse a project with nested source directives
    let kconfig_path = PathBuf::from("examples/sample_project/Kconfig");
    let srctree = PathBuf::from("examples/sample_project");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let ast = parser.parse().unwrap();

    // Should successfully parse all entries from main and sourced files
    assert!(ast.entries.len() >= 6);

    // Verify we got entries from sourced files
    let has_x86 = ast.entries.iter().any(|entry| match entry {
        xconfig::kconfig::Entry::Menu(menu) => menu.entries.iter().any(|e| match e {
            xconfig::kconfig::Entry::Config(config) => config.name == "X86",
            _ => false,
        }),
        _ => false,
    });
    assert!(has_x86);
}

#[test]
fn test_config_prefix_removal() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("test.config");
    let auto_conf_path = temp_dir.path().join("auto.conf");
    let autoconf_h_path = temp_dir.path().join("autoconf.h");

    // Create symbols with CONFIG_ prefix
    let mut symbols = SymbolTable::new();
    symbols.add_symbol("CONFIG_TEST1".to_string(), SymbolType::Bool);
    symbols.set_value("CONFIG_TEST1", "y".to_string());
    symbols.add_symbol("CONFIG_TEST2".to_string(), SymbolType::String);
    symbols.set_value("CONFIG_TEST2", "value".to_string());

    // Write files
    ConfigWriter::write(&config_path, &symbols).unwrap();
    ConfigGenerator::generate_auto_conf(&auto_conf_path, &symbols).unwrap();
    ConfigGenerator::generate_autoconf_h(&autoconf_h_path, &symbols).unwrap();

    // Verify CONFIG_ prefix is removed from all files
    let config_content = std::fs::read_to_string(&config_path).unwrap();
    assert!(config_content.contains("TEST1=y"));
    assert!(config_content.contains("TEST2=\"value\""));
    assert!(!config_content.contains("CONFIG_"));

    let auto_conf_content = std::fs::read_to_string(&auto_conf_path).unwrap();
    assert!(auto_conf_content.contains("TEST1=y"));
    assert!(!auto_conf_content.contains("CONFIG_"));

    let autoconf_h_content = std::fs::read_to_string(&autoconf_h_path).unwrap();
    assert!(autoconf_h_content.contains("#define TEST1 1"));
    assert!(!autoconf_h_content.contains("CONFIG_"));
}

#[test]
fn test_oldconfig_no_changes() {
    let kconfig_path = PathBuf::from("tests/fixtures/test_oldconfig/Kconfig_v1");
    let config_path = PathBuf::from("tests/fixtures/test_oldconfig/.config_v1");
    let srctree = PathBuf::from("tests/fixtures/test_oldconfig");

    let loader = OldConfigLoader::new(&kconfig_path, &srctree);
    let (symbols, changes) = loader.load_and_merge(&config_path).unwrap();

    // No new or removed symbols
    assert!(!changes.has_changes());
    assert_eq!(changes.new_symbols.len(), 0);
    assert_eq!(changes.removed_symbols.len(), 0);

    // Values should be preserved from old config
    assert_eq!(symbols.get_value("OPTION_A"), Some("y".to_string()));
    assert_eq!(symbols.get_value("OPTION_B"), Some("world".to_string()));
}

#[test]
fn test_oldconfig_new_symbols() {
    let kconfig_path = PathBuf::from("tests/fixtures/test_oldconfig/Kconfig_v2");
    let config_path = PathBuf::from("tests/fixtures/test_oldconfig/.config_v1");
    let srctree = PathBuf::from("tests/fixtures/test_oldconfig");

    let loader = OldConfigLoader::new(&kconfig_path, &srctree);
    let (symbols, changes) = loader.load_and_merge(&config_path).unwrap();

    // Should detect one new symbol
    assert!(changes.has_changes());
    assert_eq!(changes.new_symbols.len(), 1);
    assert!(changes.new_symbols.contains(&"OPTION_C".to_string()));
    assert_eq!(changes.removed_symbols.len(), 0);

    // Old values should be preserved
    assert_eq!(symbols.get_value("OPTION_A"), Some("y".to_string()));
    assert_eq!(symbols.get_value("OPTION_B"), Some("world".to_string()));

    // New symbol should be marked as new
    let option_c = symbols.get_symbol("OPTION_C").unwrap();
    assert!(option_c.is_new);
}

#[test]
fn test_oldconfig_removed_symbols() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("old.config");

    // Create old config with extra symbols
    std::fs::write(
        &config_path,
        "OPTION_A=y\nOPTION_B=\"test\"\nOLD_SYMBOL=y\n",
    )
    .unwrap();

    let kconfig_path = PathBuf::from("tests/fixtures/test_oldconfig/Kconfig_v1");
    let srctree = PathBuf::from("tests/fixtures/test_oldconfig");

    let loader = OldConfigLoader::new(&kconfig_path, &srctree);
    let (symbols, changes) = loader.load_and_merge(&config_path).unwrap();

    // Should detect one removed symbol
    assert!(changes.has_changes());
    assert_eq!(changes.new_symbols.len(), 0);
    assert_eq!(changes.removed_symbols.len(), 1);
    assert!(changes.removed_symbols.contains(&"OLD_SYMBOL".to_string()));

    // Removed symbol should not be in symbol table
    assert!(symbols.get_symbol("OLD_SYMBOL").is_none());

    // Valid symbols should be preserved
    assert_eq!(symbols.get_value("OPTION_A"), Some("y".to_string()));
    assert_eq!(symbols.get_value("OPTION_B"), Some("test".to_string()));
}

#[test]
fn test_change_tracking() {
    let mut symbols = SymbolTable::new();
    symbols.add_symbol("TEST1".to_string(), SymbolType::Bool);
    symbols.add_symbol("TEST2".to_string(), SymbolType::String);

    // Set initial values
    symbols.set_value("TEST1", "n".to_string());
    symbols.set_value("TEST2", "initial".to_string());

    // Track changes
    symbols.set_value_tracked("TEST1", "y".to_string());
    symbols.set_value_tracked("TEST2", "changed".to_string());

    // Verify changes are tracked
    let changed = symbols.get_changed_symbols();
    assert_eq!(changed.len(), 2);
    assert!(changed.contains(&"TEST1".to_string()));
    assert!(changed.contains(&"TEST2".to_string()));

    // Values should be updated
    assert_eq!(symbols.get_value("TEST1"), Some("y".to_string()));
    assert_eq!(symbols.get_value("TEST2"), Some("changed".to_string()));
}

#[test]
fn test_full_workflow() {
    let temp_dir = TempDir::new().unwrap();

    // 1. Create initial Kconfig and .config
    let kconfig_v1 = temp_dir.path().join("Kconfig_v1");
    std::fs::write(
        &kconfig_v1,
        "config TEST_A\n    bool \"Test A\"\n    default y\n",
    )
    .unwrap();

    let config_v1 = temp_dir.path().join(".config_v1");
    std::fs::write(&config_v1, "TEST_A=y\n").unwrap();

    // 2. Read and verify
    let config_data = ConfigReader::read(&config_v1).unwrap();
    assert_eq!(config_data.get("TEST_A"), Some(&"y".to_string()));

    // 3. Modify Kconfig (add new symbol)
    let kconfig_v2 = temp_dir.path().join("Kconfig_v2");
    std::fs::write(
        &kconfig_v2,
        "config TEST_A\n    bool \"Test A\"\n    default y\n\nconfig TEST_B\n    bool \"Test \
         B\"\n    default n\n",
    )
    .unwrap();

    // 4. Run oldconfig
    let loader = OldConfigLoader::new(&kconfig_v2, temp_dir.path());
    let (mut symbols, changes) = loader.load_and_merge(&config_v1).unwrap();

    // 5. Verify changes detected
    assert!(changes.has_changes());
    assert_eq!(changes.new_symbols.len(), 1);
    assert!(changes.new_symbols.contains(&"TEST_B".to_string()));

    // 6. Apply defaults to new symbols
    for name in &changes.new_symbols {
        if let Some(symbol) = symbols.get_symbol(name) {
            if symbol.value.is_none() {
                symbols.set_value(name, "n".to_string());
            }
        }
    }

    // 7. Save new config
    let config_v2 = temp_dir.path().join(".config_v2");
    ConfigWriter::write(&config_v2, &symbols).unwrap();

    // 8. Verify output
    let content = std::fs::read_to_string(&config_v2).unwrap();
    assert!(content.contains("TEST_A=y"));
    assert!(content.contains("# TEST_B is not set"));
}
