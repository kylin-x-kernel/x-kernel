// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::fs;

use tempfile::TempDir;
use xconfig::{
    config::writer::ConfigWriter,
    kconfig::{SymbolTable, SymbolType, ast::RangeType},
};

#[test]
fn test_config_stable_ordering() {
    let temp_dir = TempDir::new().unwrap();

    // Create symbol table with multiple symbols in random insertion order
    let mut symbols = SymbolTable::new();

    symbols.add_symbol("ZZZ_LAST".to_string(), SymbolType::String);
    symbols.set_value("ZZZ_LAST", "\"last\"".to_string());

    symbols.add_symbol("AAA_FIRST".to_string(), SymbolType::String);
    symbols.set_value("AAA_FIRST", "\"first\"".to_string());

    symbols.add_symbol("MMM_MIDDLE".to_string(), SymbolType::U32);
    symbols.set_value("MMM_MIDDLE", "42".to_string());

    symbols.add_symbol(
        "BBB_SECOND".to_string(),
        SymbolType::Range(RangeType::Unknown),
    );
    symbols.set_value("BBB_SECOND", "[1, 2, 3]".to_string());

    // Generate config twice
    let config1_path = temp_dir.path().join("config1");
    let config2_path = temp_dir.path().join("config2");

    ConfigWriter::write(&config1_path, &symbols).unwrap();
    ConfigWriter::write(&config2_path, &symbols).unwrap();

    // Read both files
    let content1 = fs::read_to_string(&config1_path).unwrap();
    let content2 = fs::read_to_string(&config2_path).unwrap();

    // Check if they are identical
    assert_eq!(
        content1, content2,
        "Configs generated twice should be identical"
    );

    // Verify AAA comes before BBB comes before MMM comes before ZZZ
    let aaa_pos = content1.find("AAA_FIRST").unwrap();
    let bbb_pos = content1.find("BBB_SECOND").unwrap();
    let mmm_pos = content1.find("MMM_MIDDLE").unwrap();
    let zzz_pos = content1.find("ZZZ_LAST").unwrap();

    assert!(aaa_pos < bbb_pos, "AAA_FIRST should come before BBB_SECOND");
    assert!(
        bbb_pos < mmm_pos,
        "BBB_SECOND should come before MMM_MIDDLE"
    );
    assert!(mmm_pos < zzz_pos, "MMM_MIDDLE should come before ZZZ_LAST");
}

#[test]
fn test_config_ordering_with_config_prefix() {
    let temp_dir = TempDir::new().unwrap();

    // Create symbol table with CONFIG_ prefixes
    let mut symbols = SymbolTable::new();

    symbols.add_symbol("CONFIG_ZZZZZ".to_string(), SymbolType::Bool);
    symbols.set_value("CONFIG_ZZZZZ", "y".to_string());

    symbols.add_symbol("CONFIG_AAAAA".to_string(), SymbolType::Bool);
    symbols.set_value("CONFIG_AAAAA", "y".to_string());

    symbols.add_symbol("CONFIG_MMMMM".to_string(), SymbolType::Bool);
    symbols.set_value("CONFIG_MMMMM", "y".to_string());

    let config_path = temp_dir.path().join("config");
    ConfigWriter::write(&config_path, &symbols).unwrap();

    let content = fs::read_to_string(&config_path).unwrap();

    // Verify ordering (CONFIG_ prefix should be stripped in output)
    let aaa_pos = content.find("AAAAA").unwrap();
    let mmm_pos = content.find("MMMMM").unwrap();
    let zzz_pos = content.find("ZZZZZ").unwrap();

    assert!(aaa_pos < mmm_pos, "AAAAA should come before MMMMM");
    assert!(mmm_pos < zzz_pos, "MMMMM should come before ZZZZZ");
}
