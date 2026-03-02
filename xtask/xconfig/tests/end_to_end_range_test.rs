use std::fs;

use tempfile::TempDir;
use xconfig::{
    config::{ConfigReader, ConfigWriter},
    kconfig::{
        Parser, SymbolTable, SymbolType,
        ast::{Entry, RangeType, RustType},
    },
};

#[test]
fn test_explicit_tuple_type_annotation() {
    let temp_dir = TempDir::new().unwrap();

    // Create test Kconfig file with explicit type annotation
    let kconfig_content = r#"
mainmenu "Test"

config TEST_MMIO_BASE
    rangetype "MMIO Base Address"
    default [(usize, usize)]
"#;
    let kconfig_path = temp_dir.path().join("Kconfig");
    fs::write(&kconfig_path, kconfig_content).unwrap();

    // Parse Kconfig
    let mut parser = Parser::new(&kconfig_path, temp_dir.path()).unwrap();
    let parse_result = parser.parse();

    if let Err(e) = &parse_result {
        eprintln!("Parse error: {}", e);
    }
    assert!(
        parse_result.is_ok(),
        "Failed to parse Kconfig with tuple type annotation"
    );
    let ast = parse_result.unwrap();

    // Find config and verify type annotation
    let config = ast.entries.iter().find_map(|entry| {
        if let Entry::Config(config) = entry {
            if config.name == "TEST_MMIO_BASE" {
                return Some(config);
            }
        }
        None
    });
    assert!(config.is_some(), "TEST_MMIO_BASE config not found");
    let config = config.unwrap();

    match &config.symbol_type {
        SymbolType::Range(RangeType::Tuple(types)) => {
            assert_eq!(types.len(), 2);
            assert!(matches!(types[0], RustType::Usize));
            assert!(matches!(types[1], RustType::Usize));
        }
        other => panic!("Expected Range(Tuple([Usize, Usize])), got {:?}", other),
    }
    // No concrete default values — type annotation is in symbol_type
    assert_eq!(config.properties.defaults.len(), 0);
}

#[test]
fn test_primitive_type_annotation() {
    let temp_dir = TempDir::new().unwrap();

    let kconfig_content = r#"
mainmenu "Test"

config TEST_IRQ_LIST
    rangetype "IRQ Numbers"
    default [u32]
"#;
    let kconfig_path = temp_dir.path().join("Kconfig");
    fs::write(&kconfig_path, kconfig_content).unwrap();

    let mut parser = Parser::new(&kconfig_path, temp_dir.path()).unwrap();
    let ast = parser.parse().unwrap();

    let config = ast.entries.iter().find_map(|entry| {
        if let Entry::Config(config) = entry {
            if config.name == "TEST_IRQ_LIST" {
                return Some(config);
            }
        }
        None
    });
    assert!(config.is_some());
    let config = config.unwrap();

    assert!(
        matches!(
            &config.symbol_type,
            SymbolType::Range(RangeType::Primitive(RustType::U32))
        ),
        "Expected Range(Primitive(U32)), got {:?}",
        config.symbol_type
    );
}

#[test]
fn test_string_array_type_annotation() {
    let temp_dir = TempDir::new().unwrap();

    let kconfig_content = r#"
mainmenu "Test"

config TEST_NAMES
    rangetype "Names"
    default ["&str"]
"#;
    let kconfig_path = temp_dir.path().join("Kconfig");
    fs::write(&kconfig_path, kconfig_content).unwrap();

    let mut parser = Parser::new(&kconfig_path, temp_dir.path()).unwrap();
    let ast = parser.parse().unwrap();

    let config = ast.entries.iter().find_map(|entry| {
        if let Entry::Config(config) = entry {
            if config.name == "TEST_NAMES" {
                return Some(config);
            }
        }
        None
    });
    assert!(config.is_some());
    let config = config.unwrap();

    assert!(
        matches!(
            &config.symbol_type,
            SymbolType::Range(RangeType::StringArray)
        ),
        "Expected Range(StringArray), got {:?}",
        config.symbol_type
    );
}

#[test]
fn test_empty_bracket_is_error() {
    let temp_dir = TempDir::new().unwrap();

    let kconfig_content = r#"
mainmenu "Test"

config TEST_EMPTY
    rangetype "Test"
    default []
"#;
    let kconfig_path = temp_dir.path().join("Kconfig");
    fs::write(&kconfig_path, kconfig_content).unwrap();

    let mut parser = Parser::new(&kconfig_path, temp_dir.path()).unwrap();
    let result = parser.parse();
    assert!(result.is_err(), "Empty [] should be a parse error");
}

#[test]
fn test_range_config_write_read_roundtrip() {
    let temp_dir = TempDir::new().unwrap();

    // Build a symbol table manually (simulating loaded config)
    let mut symbols = SymbolTable::new();
    symbols.add_symbol(
        "TEST_RANGES".to_string(),
        SymbolType::Range(RangeType::Tuple(vec![RustType::Usize, RustType::Usize])),
    );
    symbols.set_value("TEST_RANGES", "[(0x1000_0000,0x2000_0000)]".to_string());

    // Write to .config
    let config_path = temp_dir.path().join(".config");
    ConfigWriter::write(&config_path, &symbols).unwrap();

    // Verify .config content — ranges should not have extra quotes
    let config_content = fs::read_to_string(&config_path).unwrap();
    assert!(
        config_content.contains("TEST_RANGES=[(0x1000_0000,0x2000_0000)]"),
        "Config file should contain range values without extra quotes. Got: {}",
        config_content
    );
    assert!(
        !config_content.contains("\"["),
        "Config should not have extra quotes around arrays"
    );

    // Read .config back
    let config = ConfigReader::read(&config_path).unwrap();
    assert_eq!(
        config.get("TEST_RANGES"),
        Some(&"[(0x1000_0000,0x2000_0000)]".to_string())
    );
}

#[test]
fn test_hex_array_mixed_formats() {
    let temp_dir = TempDir::new().unwrap();

    // Kconfig with explicit tuple type annotation
    let kconfig_content = r#"
mainmenu "Test"

config TEST_HEX_MIXED
    rangetype "Hex Mixed Format"
    default [(usize, usize)]
"#;
    let kconfig_path = temp_dir.path().join("Kconfig");
    fs::write(&kconfig_path, kconfig_content).unwrap();

    let mut parser = Parser::new(&kconfig_path, temp_dir.path()).unwrap();
    let parse_result = parser.parse();

    if let Err(e) = &parse_result {
        eprintln!("Parse error: {}", e);
    }
    assert!(
        parse_result.is_ok(),
        "Failed to parse Kconfig with tuple type annotation"
    );
    let ast = parse_result.unwrap();

    let config = ast.entries.iter().find_map(|entry| {
        if let Entry::Config(config) = entry {
            if config.name == "TEST_HEX_MIXED" {
                return Some(config);
            }
        }
        None
    });

    assert!(config.is_some(), "TEST_HEX_MIXED config not found");
    let config = config.unwrap();

    // Verify type annotation is (usize, usize)
    match &config.symbol_type {
        SymbolType::Range(RangeType::Tuple(types)) => {
            assert_eq!(types.len(), 2);
            assert!(matches!(types[0], RustType::Usize));
            assert!(matches!(types[1], RustType::Usize));
        }
        other => panic!("Expected Range(Tuple([Usize, Usize])), got {:?}", other),
    }
}
