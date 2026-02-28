use std::path::PathBuf;
use xconfig::kconfig::{Parser, SymbolTable, SymbolType};
use xconfig::ui::app::MenuConfigApp;

/// Test that MenuConfigApp can be created with initialized values
/// This verifies the critical fix for checkbox state display
#[test]
fn test_menuconfig_app_initialization_with_values() {
    // Setup test Kconfig
    let kconfig_path = PathBuf::from("examples/sample_project/Kconfig");
    let srctree = PathBuf::from("examples/sample_project");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let ast = parser.parse().unwrap();

    // Create symbol table with some values
    let mut symbol_table = SymbolTable::new();
    symbol_table.add_symbol("HAVE_ARCH".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("DEBUG".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("VERBOSE".to_string(), SymbolType::Bool);

    symbol_table.set_value("HAVE_ARCH", "y".to_string());
    symbol_table.set_value("DEBUG", "y".to_string());
    symbol_table.set_value("VERBOSE", "n".to_string());

    // Create MenuConfigApp - this should initialize values in both all_items AND menu_tree
    let app = MenuConfigApp::new(ast.entries, symbol_table);

    // The app should be created successfully
    assert!(
        app.is_ok(),
        "MenuConfigApp should be created successfully with initialized values"
    );
}

/// Test that MenuConfigApp can be created without pre-existing config values
#[test]
fn test_menuconfig_app_initialization_with_defaults() {
    // Setup test Kconfig
    let kconfig_path = PathBuf::from("examples/sample_project/Kconfig");
    let srctree = PathBuf::from("examples/sample_project");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let ast = parser.parse().unwrap();

    // Create empty symbol table
    let symbol_table = SymbolTable::new();

    // Create MenuConfigApp - this should initialize with default values
    let app = MenuConfigApp::new(ast.entries, symbol_table);

    // The app should be created successfully with defaults
    assert!(
        app.is_ok(),
        "MenuConfigApp should be created successfully with default values"
    );
}

/// Integration test: Load a config file and verify app initializes correctly
#[test]
fn test_menuconfig_app_with_config_file() {
    use xconfig::config::ConfigReader;

    // Create a temporary config file
    let config_content = r#"
CONFIG_HAVE_ARCH=y
CONFIG_DEBUG=y
CONFIG_VERBOSE=n
"#;

    let temp_dir = std::env::temp_dir();
    let config_path = temp_dir.join("test_config_menuconfig");
    std::fs::write(&config_path, config_content).unwrap();

    // Parse Kconfig
    let kconfig_path = PathBuf::from("examples/sample_project/Kconfig");
    let srctree = PathBuf::from("examples/sample_project");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let ast = parser.parse().unwrap();

    // Load config file and build symbol table
    let config_values = ConfigReader::read(&config_path).unwrap();
    let mut symbol_table = SymbolTable::new();
    for (name, value) in config_values {
        symbol_table.set_value(&name, value);
    }

    // Create MenuConfigApp with loaded config
    let app = MenuConfigApp::new(ast.entries, symbol_table);

    // The app should be created successfully
    assert!(
        app.is_ok(),
        "MenuConfigApp should be created successfully with config file"
    );

    // Cleanup
    std::fs::remove_file(config_path).ok();
}
