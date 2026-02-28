use std::fs;
use tempfile::TempDir;
use xconfig::config::writer::ConfigWriter;
use xconfig::kconfig::{SymbolTable, SymbolType};
use xconfig::kconfig::ast::RangeType;

#[test]
fn test_range_config_writer() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("test.config");

    let mut symbols = SymbolTable::new();

    // Add range configs with different element types
    symbols.add_symbol("TEST_RANGE_NUMBERS".to_string(), SymbolType::Range(RangeType::Unknown));
    symbols.set_value("TEST_RANGE_NUMBERS", "[1,2,3,4,5]".to_string());

    symbols.add_symbol("TEST_RANGE_STRINGS".to_string(), SymbolType::Range(RangeType::Unknown));
    symbols.set_value("TEST_RANGE_STRINGS", "[apple,banana,cherry]".to_string());

    symbols.add_symbol("TEST_RANGE_HEX".to_string(), SymbolType::Range(RangeType::Unknown));
    symbols.set_value("TEST_RANGE_HEX", "[0x10,0x20,0x30]".to_string());

    symbols.add_symbol("TEST_RANGE_EMPTY".to_string(), SymbolType::Range(RangeType::Unknown));
    symbols.set_value("TEST_RANGE_EMPTY", "[]".to_string());

    // Add a normal u32 to verify no regression
    symbols.add_symbol("TEST_NORMAL_INT".to_string(), SymbolType::U32);
    symbols.set_value("TEST_NORMAL_INT", "42".to_string());

    ConfigWriter::write(&config_path, &symbols).unwrap();

    // Read back and verify
    let content = fs::read_to_string(&config_path).unwrap();

    // Range values should be written without extra quotes
    assert!(
        content.contains("TEST_RANGE_NUMBERS=[1,2,3,4,5]"),
        "Range should be written without quotes"
    );
    assert!(
        content.contains("TEST_RANGE_STRINGS=[apple,banana,cherry]"),
        "Range should be written without quotes"
    );
    assert!(
        content.contains("TEST_RANGE_HEX=[0x10,0x20,0x30]"),
        "Range should be written without quotes"
    );
    assert!(
        content.contains("TEST_RANGE_EMPTY=[]"),
        "Empty range should be written correctly"
    );
    assert!(
        content.contains("TEST_NORMAL_INT=42"),
        "Normal u32 should still work"
    );

    // Verify no unwanted quotes around ranges
    assert!(
        !content.contains("\"["),
        "Range should not have quotes around brackets"
    );
}

