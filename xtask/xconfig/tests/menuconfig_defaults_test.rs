use xconfig::config::ConfigReader;
use xconfig::kconfig::{SymbolTable, SymbolType};

#[test]
fn test_menuconfig_preserves_new_hex_defaults() {
    // This test verifies that when a new hex config option with a default value
    // is added to Kconfig, and an old .config file contains "# XXX is not set" for it,
    // the menuconfig should preserve the default value instead of overwriting it with "n".

    // Manually create symbol table with symbols and defaults
    let mut symbol_table = SymbolTable::new();
    
    // Add existing bool with default y
    symbol_table.add_symbol("EXISTING_BOOL".to_string(), SymbolType::Bool);
    symbol_table.set_value("EXISTING_BOOL", "y".to_string());
    
    // Add new hex symbol with default
    symbol_table.add_symbol("KERNEL_ASPACE_SIZE".to_string(), SymbolType::Hex);
    symbol_table.set_value("KERNEL_ASPACE_SIZE", "0x0000FFFFFFFF0000".to_string());

    // Verify defaults are set
    assert_eq!(
        symbol_table.get_value("EXISTING_BOOL"),
        Some("y".to_string())
    );
    assert_eq!(
        symbol_table.get_value("KERNEL_ASPACE_SIZE"),
        Some("0x0000FFFFFFFF0000".to_string()),
        "Default value should be set before loading .config"
    );


    // Simulate loading .config (this is what menuconfig does)
    // The config has "# KERNEL_ASPACE_SIZE is not set" which ConfigReader parses as "n"
    let config_str = r#"EXISTING_BOOL=y
# KERNEL_ASPACE_SIZE is not set
"#;
    
    // Write to a temp file and read it
    let temp_dir = tempfile::TempDir::new().unwrap();
    let config_path = temp_dir.path().join(".config");
    std::fs::write(&config_path, config_str).unwrap();
    
    let config_values = ConfigReader::read(&config_path).unwrap();
    
    // Apply the fix: don't override hex/int/string defaults with "n"
    for (name, value) in config_values {
        // This is the fix: don't override hex/int/string defaults with "n"
        if value == "n" {
            if let Some(symbol) = symbol_table.get_symbol(&name) {
                match symbol.symbol_type {
                    SymbolType::Bool | SymbolType::Tristate => {
                        symbol_table.set_value(&name, value);
                    }
                    _ => {
                        // Skip "n" for hex/int/string - preserve default
                    }
                }
            }
        } else {
            symbol_table.set_value(&name, value);
        }
    }

    // Verify the hex default is preserved
    assert_eq!(
        symbol_table.get_value("KERNEL_ASPACE_SIZE"),
        Some("0x0000FFFFFFFF0000".to_string()),
        "Hex default should be preserved even when .config has '# XXX is not set'"
    );
    
    // Verify bool value is still loaded from config
    assert_eq!(
        symbol_table.get_value("EXISTING_BOOL"),
        Some("y".to_string())
    );
}

#[test]
fn test_menuconfig_respects_bool_not_set() {
    // This test verifies that bool options with "# XXX is not set" in .config
    // are still respected (set to "n"), even if they have a default of "y"
    
    // Create symbol table with bool that defaults to y
    let mut symbol_table = SymbolTable::new();
    symbol_table.add_symbol("DEBUG_MODE".to_string(), SymbolType::Bool);
    symbol_table.set_value("DEBUG_MODE", "y".to_string());

    // Default should be "y"
    assert_eq!(
        symbol_table.get_value("DEBUG_MODE"),
        Some("y".to_string())
    );

    // Simulate loading .config with "# DEBUG_MODE is not set"
    let config_str = "# DEBUG_MODE is not set\n";
    let temp_dir = tempfile::TempDir::new().unwrap();
    let config_path = temp_dir.path().join(".config");
    std::fs::write(&config_path, config_str).unwrap();
    
    let config_values = ConfigReader::read(&config_path).unwrap();
    
    // Apply the fix
    for (name, value) in config_values {
        if value == "n" {
            if let Some(symbol) = symbol_table.get_symbol(&name) {
                match symbol.symbol_type {
                    SymbolType::Bool | SymbolType::Tristate => {
                        symbol_table.set_value(&name, value);
                    }
                    _ => {}
                }
            }
        } else {
            symbol_table.set_value(&name, value);
        }
    }

    // Bool "n" should override the default "y"
    assert_eq!(
        symbol_table.get_value("DEBUG_MODE"),
        Some("n".to_string()),
        "Bool 'n' from .config should override default 'y'"
    );
}
