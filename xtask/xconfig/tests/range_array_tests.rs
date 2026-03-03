// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::path::PathBuf;

use xconfig::kconfig::{
    Parser,
    ast::{Entry, RangeType, RustType, SymbolType},
};

#[test]
fn test_parse_range_array_numbers() {
    let kconfig_path = PathBuf::from("tests/fixtures/range_arrays/Kconfig");
    let srctree = PathBuf::from("tests/fixtures/range_arrays");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let result = parser.parse();

    if let Err(e) = &result {
        eprintln!("Parse error: {}", e);
    }
    assert!(result.is_ok());
    let ast = result.unwrap();

    // Find the TEST_RANGE_NUMBERS config
    let range_config = ast.entries.iter().find_map(|entry| {
        if let Entry::Config(config) = entry {
            if config.name == "TEST_RANGE_NUMBERS" {
                return Some(config);
            }
        }
        None
    });

    assert!(
        range_config.is_some(),
        "TEST_RANGE_NUMBERS config not found"
    );
    let range_config = range_config.unwrap();

    // Verify the symbol type is Range with a Primitive(U32) annotation
    assert!(
        matches!(
            &range_config.symbol_type,
            SymbolType::Range(RangeType::Primitive(RustType::U32))
        ),
        "Expected Range(Primitive(U32)), got {:?}",
        range_config.symbol_type
    );
    // No concrete default values — type annotation is stored in symbol_type
    assert_eq!(range_config.properties.defaults.len(), 0);
}

#[test]
fn test_parse_range_array_hex() {
    let kconfig_path = PathBuf::from("tests/fixtures/range_arrays/Kconfig");
    let srctree = PathBuf::from("tests/fixtures/range_arrays");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let result = parser.parse();

    assert!(result.is_ok());
    let ast = result.unwrap();

    // Find the TEST_RANGE_HEX config
    let hex_config = ast.entries.iter().find_map(|entry| {
        if let Entry::Config(config) = entry {
            if config.name == "TEST_RANGE_HEX" {
                return Some(config);
            }
        }
        None
    });

    assert!(hex_config.is_some(), "TEST_RANGE_HEX config not found");
    let hex_config = hex_config.unwrap();

    // Verify the symbol type is Range with a Primitive(Usize) annotation
    assert!(
        matches!(
            &hex_config.symbol_type,
            SymbolType::Range(RangeType::Primitive(RustType::Usize))
        ),
        "Expected Range(Primitive(Usize)), got {:?}",
        hex_config.symbol_type
    );
    assert_eq!(hex_config.properties.defaults.len(), 0);
}

#[test]
fn test_parse_range_array_identifiers() {
    let kconfig_path = PathBuf::from("tests/fixtures/range_arrays/Kconfig");
    let srctree = PathBuf::from("tests/fixtures/range_arrays");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let result = parser.parse();

    assert!(result.is_ok());
    let ast = result.unwrap();

    // Find the TEST_RANGE_IDENTIFIERS config
    let id_config = ast.entries.iter().find_map(|entry| {
        if let Entry::Config(config) = entry {
            if config.name == "TEST_RANGE_IDENTIFIERS" {
                return Some(config);
            }
        }
        None
    });

    assert!(
        id_config.is_some(),
        "TEST_RANGE_IDENTIFIERS config not found"
    );
    let id_config = id_config.unwrap();

    // Verify the symbol type is Range with a StringArray annotation
    assert!(
        matches!(
            &id_config.symbol_type,
            SymbolType::Range(RangeType::StringArray)
        ),
        "Expected Range(StringArray), got {:?}",
        id_config.symbol_type
    );
    assert_eq!(id_config.properties.defaults.len(), 0);
}

#[test]
fn test_parse_range_array_empty() {
    let kconfig_path = PathBuf::from("tests/fixtures/range_arrays/Kconfig");
    let srctree = PathBuf::from("tests/fixtures/range_arrays");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let result = parser.parse();

    assert!(result.is_ok());
    let ast = result.unwrap();

    // Find the TEST_RANGE_EMPTY config
    let empty_config = ast.entries.iter().find_map(|entry| {
        if let Entry::Config(config) = entry {
            if config.name == "TEST_RANGE_EMPTY" {
                return Some(config);
            }
        }
        None
    });

    assert!(empty_config.is_some(), "TEST_RANGE_EMPTY config not found");
    let empty_config = empty_config.unwrap();

    // Verify type annotation: [(u64, u64)]
    match &empty_config.symbol_type {
        SymbolType::Range(RangeType::Tuple(types)) => {
            assert_eq!(types.len(), 2);
            assert!(matches!(types[0], RustType::U64));
            assert!(matches!(types[1], RustType::U64));
        }
        other => panic!("Expected Range(Tuple([U64, U64])), got {:?}", other),
    }
    assert_eq!(empty_config.properties.defaults.len(), 0);
}

#[test]
fn test_parse_range_array_mixed() {
    let kconfig_path = PathBuf::from("tests/fixtures/range_arrays/Kconfig");
    let srctree = PathBuf::from("tests/fixtures/range_arrays");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let result = parser.parse();

    assert!(result.is_ok());
    let ast = result.unwrap();

    // Find the TEST_RANGE_MIXED config
    let mixed_config = ast.entries.iter().find_map(|entry| {
        if let Entry::Config(config) = entry {
            if config.name == "TEST_RANGE_MIXED" {
                return Some(config);
            }
        }
        None
    });

    assert!(mixed_config.is_some(), "TEST_RANGE_MIXED config not found");
    let mixed_config = mixed_config.unwrap();

    // Verify type annotation: [(u32, usize)]
    match &mixed_config.symbol_type {
        SymbolType::Range(RangeType::Tuple(types)) => {
            assert_eq!(types.len(), 2);
            assert!(matches!(types[0], RustType::U32));
            assert!(matches!(types[1], RustType::Usize));
        }
        other => panic!("Expected Range(Tuple([U32, Usize])), got {:?}", other),
    }
    assert_eq!(mixed_config.properties.defaults.len(), 0);
}
