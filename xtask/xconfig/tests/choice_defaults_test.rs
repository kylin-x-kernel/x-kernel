use std::fs;
use tempfile::TempDir;
use xconfig::kconfig::{Parser, SymbolTable};
use xconfig::ui::app::MenuConfigApp;

/// Test that a Choice with a default option has that option selected
#[test]
fn test_choice_with_default() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig_path = temp_dir.path().join("Kconfig");

    let kconfig_content = r#"
choice
    prompt "Select Architecture"
    default ARCH_X86_64

config ARCH_X86_64
    bool "x86_64"

config ARCH_AARCH64
    bool "aarch64"

config ARCH_RISCV64
    bool "riscv64"
endchoice
"#;

    fs::write(&kconfig_path, kconfig_content).unwrap();

    let mut parser = Parser::new(&kconfig_path, temp_dir.path()).unwrap();
    let ast = parser.parse().unwrap();

    // Extract symbols (this should apply the choice default)
    let mut symbol_table = SymbolTable::new();
    extract_symbols_from_entries(&ast.entries, &mut symbol_table);

    // The default option should be selected (value = "y")
    assert_eq!(symbol_table.get_value("ARCH_X86_64"), Some("y".to_string()));
    // Others should not be selected
    assert_ne!(
        symbol_table.get_value("ARCH_AARCH64"),
        Some("y".to_string())
    );
    assert_ne!(
        symbol_table.get_value("ARCH_RISCV64"),
        Some("y".to_string())
    );

    // Create MenuConfigApp to verify UI initialization
    let _app = MenuConfigApp::new(ast.entries, symbol_table).unwrap();
}

/// Test that a Choice without a default has first option selected
#[test]
fn test_choice_without_default() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig_path = temp_dir.path().join("Kconfig");

    let kconfig_content = r#"
choice
    prompt "Select Log Level"

config LOG_ERROR
    bool "Error"

config LOG_WARN
    bool "Warning"

config LOG_INFO
    bool "Info"
endchoice
"#;

    fs::write(&kconfig_path, kconfig_content).unwrap();

    let mut parser = Parser::new(&kconfig_path, temp_dir.path()).unwrap();
    let ast = parser.parse().unwrap();

    // Extract symbols (first option should be auto-selected)
    let mut symbol_table = SymbolTable::new();
    extract_symbols_from_entries(&ast.entries, &mut symbol_table);

    // The first option should be selected by default
    assert_eq!(symbol_table.get_value("LOG_ERROR"), Some("y".to_string()));
    // Others should not be selected
    assert_ne!(symbol_table.get_value("LOG_WARN"), Some("y".to_string()));
    assert_ne!(symbol_table.get_value("LOG_INFO"), Some("y".to_string()));

    // Create MenuConfigApp
    let _app = MenuConfigApp::new(ast.entries, symbol_table).unwrap();
}

/// Test shell expression in config default value
#[test]
fn test_shell_expression_in_default() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig_path = temp_dir.path().join("Kconfig");

    let kconfig_content = r#"
choice
    prompt "Select Architecture"
    default ARCH_AARCH64

config ARCH_AARCH64
    bool "aarch64"

config ARCH_RISCV64
    bool "riscv64"

config ARCH_X86_64
    bool "x86_64"
endchoice

config ARCH_NAME
    string
    default "$(if $(ARCH_AARCH64),aarch64,$(if $(ARCH_RISCV64),riscv64,$(if $(ARCH_X86_64),x86_64,unknown)))"
"#;

    fs::write(&kconfig_path, kconfig_content).unwrap();

    let mut parser = Parser::new(&kconfig_path, temp_dir.path()).unwrap();
    let ast = parser.parse().unwrap();

    // Extract symbols
    let mut symbol_table = SymbolTable::new();
    extract_symbols_from_entries(&ast.entries, &mut symbol_table);

    // ARCH_AARCH64 should be selected
    assert_eq!(
        symbol_table.get_value("ARCH_AARCH64"),
        Some("y".to_string())
    );

    // ARCH_NAME should evaluate to "aarch64" since ARCH_AARCH64 is selected
    assert_eq!(
        symbol_table.get_value("ARCH_NAME"),
        Some("aarch64".to_string())
    );
}

/// Helper function to extract symbols (same as in menuconfig.rs)
fn extract_symbols_from_entries(
    entries: &[xconfig::kconfig::ast::Entry],
    symbol_table: &mut SymbolTable,
) {
    use xconfig::kconfig::Expr;
    use xconfig::kconfig::ast::Entry;

    for entry in entries {
        match entry {
            Entry::Config(config) => {
                symbol_table.add_symbol(config.name.clone(), config.symbol_type.clone());

                // Evaluate conditional defaults
                for default in &config.properties.defaults {
                    let should_apply = if let Some(ref cond) = default.condition {
                        // Evaluate the condition
                        xconfig::kconfig::expr::evaluate_expr(cond, symbol_table).unwrap_or(false)
                    } else {
                        // Unconditional default (always apply if no previous default matched)
                        true
                    };

                    if should_apply {
                        // Apply this default and stop checking further defaults
                        let mut applied = false;
                        if let Expr::Const(val) = &default.value {
                            symbol_table.set_value(&config.name, val.clone());
                            applied = true;
                        } else if let Expr::Symbol(sym) = &default.value {
                            symbol_table.set_value(&config.name, sym.clone());
                            applied = true;
                        } else if let Expr::ShellExpr(shell_expr) = &default.value {
                            if let Ok(value) = xconfig::kconfig::shell_expr::evaluate_shell_expr(
                                shell_expr,
                                symbol_table,
                            ) {
                                if !value.is_empty() {
                                    symbol_table.set_value(&config.name, value);
                                    applied = true;
                                }
                            }
                        }

                        if applied {
                            break; // Stop at first matching default that was successfully applied
                        }
                    }
                }
            }
            Entry::MenuConfig(menuconfig) => {
                symbol_table.add_symbol(menuconfig.name.clone(), menuconfig.symbol_type.clone());
            }
            Entry::Choice(choice) => {
                for option in &choice.options {
                    symbol_table.add_symbol(option.name.clone(), option.symbol_type.clone());
                }

                if let Some(default_name) = &choice.default {
                    symbol_table.set_value(default_name, "y".to_string());
                } else if let Some(first_option) = choice.options.first() {
                    symbol_table.set_value(&first_option.name, "y".to_string());
                }
            }
            Entry::Menu(menu) => {
                extract_symbols_from_entries(&menu.entries, symbol_table);
            }
            Entry::If(if_entry) => {
                extract_symbols_from_entries(&if_entry.entries, symbol_table);
            }
            _ => {}
        }
    }
}
