use std::fs;

use tempfile::TempDir;
use xconfig::{
    config::{ConfigReader, ConfigWriter},
    kconfig::{SymbolTable, SymbolType, ast::RangeType},
};

#[test]
fn test_config_reader() {
    let config_path = "tests/fixtures/basic/expected.config";
    let config = ConfigReader::read(config_path).unwrap();

    assert_eq!(config.get("TEST_BOOL"), Some(&"y".to_string()));
    assert_eq!(config.get("TEST_STRING"), Some(&"hello".to_string()));
    assert_eq!(config.get("TEST_INT"), Some(&"42".to_string()));
}

#[test]
fn test_config_reader_backward_compat() {
    // Test reading config with CONFIG_ prefix (backward compatibility)
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("test.config");

    fs::write(
        &config_path,
        "CONFIG_TEST1=y\nCONFIG_TEST2=\"value\"\n# CONFIG_TEST3 is not set\n",
    )
    .unwrap();

    let config = ConfigReader::read(&config_path).unwrap();
    assert_eq!(config.get("TEST1"), Some(&"y".to_string()));
    assert_eq!(config.get("TEST2"), Some(&"value".to_string()));
    assert_eq!(config.get("TEST3"), Some(&"n".to_string()));
}

#[test]
fn test_config_writer() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("test.config");

    let mut symbols = SymbolTable::new();
    symbols.add_symbol("TEST1".to_string(), SymbolType::Bool);
    symbols.set_value("TEST1", "y".to_string());
    symbols.add_symbol("TEST2".to_string(), SymbolType::String);
    symbols.set_value("TEST2", "value".to_string());

    ConfigWriter::write(&config_path, &symbols).unwrap();

    // Read back and verify - should NOT have CONFIG_ prefix
    let content = fs::read_to_string(&config_path).unwrap();
    assert!(content.contains("TEST1=y"));
    assert!(content.contains("TEST2=\"value\""));
    assert!(!content.contains("CONFIG_"));
}

#[test]
fn test_config_roundtrip() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("test.config");

    // Write config
    let mut symbols = SymbolTable::new();
    symbols.add_symbol("A".to_string(), SymbolType::Bool);
    symbols.set_value("A", "y".to_string());
    symbols.add_symbol("B".to_string(), SymbolType::Bool);
    symbols.set_value("B", "n".to_string());

    ConfigWriter::write(&config_path, &symbols).unwrap();

    // Read back
    let config = ConfigReader::read(&config_path).unwrap();
    assert_eq!(config.get("A"), Some(&"y".to_string()));
    assert_eq!(config.get("B"), Some(&"n".to_string()));
}

#[test]
fn test_config_writer_int_no_quotes() {
    // Test that int values are written without quotes
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("test.config");

    let mut symbols = SymbolTable::new();
    symbols.add_symbol("MAX_CPUS".to_string(), SymbolType::U32);
    symbols.set_value("MAX_CPUS", "102".to_string());
    symbols.add_symbol("LOG_LEVEL".to_string(), SymbolType::U32);
    symbols.set_value("LOG_LEVEL", "4".to_string());

    ConfigWriter::write(&config_path, &symbols).unwrap();

    let content = fs::read_to_string(&config_path).unwrap();
    assert!(
        content.contains("MAX_CPUS=102"),
        "Int should not have quotes"
    );
    assert!(
        content.contains("LOG_LEVEL=4"),
        "Int should not have quotes"
    );
    assert!(!content.contains("\"102\""), "Int should not have quotes");
    assert!(!content.contains("\"4\""), "Int should not have quotes");
}

#[test]
fn test_config_writer_hex_no_quotes() {
    // Test that hex values are written without quotes in 0x format
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("test.config");

    let mut symbols = SymbolTable::new();

    // Test hex value starting with 0x
    symbols.add_symbol("MEMORY_BASE".to_string(), SymbolType::Hex);
    symbols.set_value("MEMORY_BASE", "0x80000000".to_string());

    // Test hex value as decimal number (should be converted to hex)
    symbols.add_symbol("MEMORY_SIZE".to_string(), SymbolType::Hex);
    symbols.set_value("MEMORY_SIZE", "2147483648".to_string());

    ConfigWriter::write(&config_path, &symbols).unwrap();

    let content = fs::read_to_string(&config_path).unwrap();
    assert!(
        content.contains("MEMORY_BASE=0x80000000"),
        "Hex should be in 0x format without quotes"
    );
    assert!(
        content.contains("MEMORY_SIZE=0x80000000"),
        "Decimal should be converted to hex format"
    );
    assert!(!content.contains("\"0x"), "Hex should not have quotes");
    assert!(
        !content.contains("\"2147483648\""),
        "Decimal input should be converted to hex"
    );
}

#[test]
fn test_config_writer_string_with_quotes() {
    // Test that string values keep quotes
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("test.config");

    let mut symbols = SymbolTable::new();
    symbols.add_symbol("KERNEL_VERSION".to_string(), SymbolType::String);
    symbols.set_value("KERNEL_VERSION", "0.0.1".to_string());
    symbols.add_symbol("BUILD_ID".to_string(), SymbolType::String);
    symbols.set_value("BUILD_ID", "abc123".to_string());

    ConfigWriter::write(&config_path, &symbols).unwrap();

    let content = fs::read_to_string(&config_path).unwrap();
    assert!(
        content.contains("KERNEL_VERSION=\"0.0.1\""),
        "String should have quotes"
    );
    assert!(
        content.contains("BUILD_ID=\"abc123\""),
        "String should have quotes"
    );
}

#[test]
fn test_config_writer_all_types() {
    // Test all types together
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("test.config");

    let mut symbols = SymbolTable::new();

    // Bool types
    symbols.add_symbol("ARM64".to_string(), SymbolType::Bool);
    symbols.set_value("ARM64", "y".to_string());

    // Int type
    symbols.add_symbol("MAX_CPUS".to_string(), SymbolType::U32);
    symbols.set_value("MAX_CPUS", "102".to_string());

    // Hex type
    symbols.add_symbol("MEMORY_BASE".to_string(), SymbolType::Hex);
    symbols.set_value("MEMORY_BASE", "2147483648".to_string());

    // String type
    symbols.add_symbol("KERNEL_VERSION".to_string(), SymbolType::String);
    symbols.set_value("KERNEL_VERSION", "0.0.1".to_string());

    ConfigWriter::write(&config_path, &symbols).unwrap();

    let content = fs::read_to_string(&config_path).unwrap();

    // Verify format
    assert!(content.contains("ARM64=y"), "Bool should not have quotes");
    assert!(
        content.contains("MAX_CPUS=102"),
        "Int should not have quotes"
    );
    assert!(
        content.contains("MEMORY_BASE=0x80000000"),
        "Hex should be in 0x format without quotes"
    );
    assert!(
        content.contains("KERNEL_VERSION=\"0.0.1\""),
        "String should have quotes"
    );

    // Verify no unwanted quotes
    assert!(
        !content.contains("MAX_CPUS=\""),
        "Int should not have quotes"
    );
    assert!(
        !content.contains("MEMORY_BASE=\""),
        "Hex should not have quotes"
    );
}

#[test]
fn test_config_reader_new_format() {
    // Test reading config with new standardized format (no quotes for int/hex)
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("test.config");

    let content = r#"
# Test config
ARM64=y
MAX_CPUS=102
MEMORY_BASE=0x80000000
LOG_LEVEL=4
KERNEL_VERSION="0.0.1"
# DEBUG is not set
"#;
    fs::write(&config_path, content).unwrap();

    let config = ConfigReader::read(&config_path).unwrap();
    assert_eq!(config.get("ARM64"), Some(&"y".to_string()));
    assert_eq!(config.get("MAX_CPUS"), Some(&"102".to_string()));
    assert_eq!(config.get("MEMORY_BASE"), Some(&"0x80000000".to_string()));
    assert_eq!(config.get("LOG_LEVEL"), Some(&"4".to_string()));
    assert_eq!(config.get("KERNEL_VERSION"), Some(&"0.0.1".to_string()));
    assert_eq!(config.get("DEBUG"), Some(&"n".to_string()));
}

#[test]
fn test_config_writer_none_values_by_type() {
    // Test that when symbol value is None:
    // - Bool/Tristate types write "# <name> is not set"
    // - Other types (String, Int, Hex, Range) skip writing
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("test.config");

    let mut symbols = SymbolTable::new();

    // Add symbols with None values
    symbols.add_symbol("BOOL_NONE".to_string(), SymbolType::Bool);
    // value is None by default

    symbols.add_symbol("TRISTATE_NONE".to_string(), SymbolType::Tristate);
    // value is None by default

    symbols.add_symbol("STRING_NONE".to_string(), SymbolType::String);
    // value is None by default

    symbols.add_symbol("INT_NONE".to_string(), SymbolType::U32);
    // value is None by default

    symbols.add_symbol("HEX_NONE".to_string(), SymbolType::Hex);
    // value is None by default

    symbols.add_symbol(
        "RANGE_NONE".to_string(),
        SymbolType::Range(RangeType::Unknown),
    );
    // value is None by default

    // Add some symbols with values for comparison
    symbols.add_symbol("BOOL_SET".to_string(), SymbolType::Bool);
    symbols.set_value("BOOL_SET", "y".to_string());

    ConfigWriter::write(&config_path, &symbols).unwrap();

    let content = fs::read_to_string(&config_path).unwrap();

    // Bool and Tristate with None should write "is not set"
    assert!(
        content.contains("# BOOL_NONE is not set"),
        "Bool with None should write 'is not set'"
    );
    assert!(
        content.contains("# TRISTATE_NONE is not set"),
        "Tristate with None should write 'is not set'"
    );

    // Other types with None should NOT appear in the config
    assert!(
        !content.contains("STRING_NONE"),
        "String with None should be skipped"
    );
    assert!(
        !content.contains("INT_NONE"),
        "Int with None should be skipped"
    );
    assert!(
        !content.contains("HEX_NONE"),
        "Hex with None should be skipped"
    );
    assert!(
        !content.contains("RANGE_NONE"),
        "Range with None should be skipped"
    );

    // Symbol with value should appear
    assert!(
        content.contains("BOOL_SET=y"),
        "Bool with value should appear"
    );
}
