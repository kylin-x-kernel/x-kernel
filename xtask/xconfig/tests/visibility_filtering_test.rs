use std::fs;

use tempfile::TempDir;
use xconfig::{
    kconfig::{Parser, SymbolTable, SymbolType},
    ui::app::MenuConfigApp,
};

/// Test that items inside `if` blocks are hidden when the condition is false
#[test]
fn test_if_condition_hides_items() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig_path = temp_dir.path().join("Kconfig");

    let kconfig_content = r#"
choice
    prompt "Architecture"
    default ARCH_X86_64

config ARCH_AARCH64
    bool "AArch64"

config ARCH_X86_64
    bool "x86_64"

endchoice

if ARCH_AARCH64

choice
    prompt "AArch64 Platform"

config PLATFORM_QEMU
    bool "QEMU"

config PLATFORM_RASPI
    bool "Raspberry Pi"

endchoice

endif

if ARCH_X86_64

config X86_FEATURE
    bool "X86 Specific Feature"

endif
"#;

    fs::write(&kconfig_path, kconfig_content).unwrap();

    let mut parser = Parser::new(&kconfig_path, temp_dir.path()).unwrap();
    let ast = parser.parse().unwrap();

    // Create symbol table with ARCH_X86_64=y (so ARCH_AARCH64 should be false)
    let mut symbol_table = SymbolTable::new();
    symbol_table.add_symbol("ARCH_AARCH64".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("ARCH_X86_64".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("PLATFORM_QEMU".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("PLATFORM_RASPI".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("X86_FEATURE".to_string(), SymbolType::Bool);

    symbol_table.set_value("ARCH_X86_64", "y".to_string());
    symbol_table.set_value("ARCH_AARCH64", "n".to_string());

    let app = MenuConfigApp::new(ast.entries, symbol_table).unwrap();

    // Get root items and filter
    let items = app.config_state().get_items_for_path(&[]);
    let visible = app.filter_visible_items(items);

    // Architecture choice should be visible
    assert!(
        visible.iter().any(|item| item.label == "Architecture"),
        "Architecture choice should be visible"
    );

    // ARCH_AARCH64 and ARCH_X86_64 options should be visible (they're part of the choice)
    assert!(
        visible.iter().any(|item| item.id == "ARCH_AARCH64"),
        "ARCH_AARCH64 should be visible as a choice option"
    );
    assert!(
        visible.iter().any(|item| item.id == "ARCH_X86_64"),
        "ARCH_X86_64 should be visible as a choice option"
    );

    // "AArch64 Platform" choice should NOT be visible (ARCH_AARCH64=n)
    assert!(
        !visible.iter().any(|item| item.label == "AArch64 Platform"),
        "AArch64 Platform choice should not be visible when ARCH_AARCH64=n"
    );

    // PLATFORM_QEMU and PLATFORM_RASPI should NOT be visible (inside if ARCH_AARCH64 block)
    assert!(
        !visible.iter().any(|item| item.id == "PLATFORM_QEMU"),
        "PLATFORM_QEMU should not be visible when ARCH_AARCH64=n"
    );
    assert!(
        !visible.iter().any(|item| item.id == "PLATFORM_RASPI"),
        "PLATFORM_RASPI should not be visible when ARCH_AARCH64=n"
    );

    // X86_FEATURE should be visible (inside if ARCH_X86_64 block, which is true)
    assert!(
        visible.iter().any(|item| item.id == "X86_FEATURE"),
        "X86_FEATURE should be visible when ARCH_X86_64=y"
    );
}

/// Test that items inside `if` blocks are shown when the condition is true
#[test]
fn test_if_condition_shows_items() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig_path = temp_dir.path().join("Kconfig");

    let kconfig_content = r#"
config ARCH_AARCH64
    bool "AArch64"
    default y

if ARCH_AARCH64

config PLATFORM_QEMU
    bool "QEMU Platform"

config PLATFORM_RASPI
    bool "Raspberry Pi Platform"

endif
"#;

    fs::write(&kconfig_path, kconfig_content).unwrap();

    let mut parser = Parser::new(&kconfig_path, temp_dir.path()).unwrap();
    let ast = parser.parse().unwrap();

    // Create symbol table with ARCH_AARCH64=y
    let mut symbol_table = SymbolTable::new();
    symbol_table.add_symbol("ARCH_AARCH64".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("PLATFORM_QEMU".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("PLATFORM_RASPI".to_string(), SymbolType::Bool);

    symbol_table.set_value("ARCH_AARCH64", "y".to_string());

    let app = MenuConfigApp::new(ast.entries, symbol_table).unwrap();

    // Get root items and filter
    let items = app.config_state().get_items_for_path(&[]);
    let visible = app.filter_visible_items(items);

    // ARCH_AARCH64 should be visible
    assert!(
        visible.iter().any(|item| item.id == "ARCH_AARCH64"),
        "ARCH_AARCH64 should be visible"
    );

    // Platform configs should be visible (inside if ARCH_AARCH64 block, which is true)
    assert!(
        visible.iter().any(|item| item.id == "PLATFORM_QEMU"),
        "PLATFORM_QEMU should be visible when ARCH_AARCH64=y"
    );
    assert!(
        visible.iter().any(|item| item.id == "PLATFORM_RASPI"),
        "PLATFORM_RASPI should be visible when ARCH_AARCH64=y"
    );
}

/// Test that configs without prompts are hidden
#[test]
fn test_no_prompt_hidden() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig_path = temp_dir.path().join("Kconfig");

    let kconfig_content = r#"
config VISIBLE_OPTION
    bool "This should be visible"

config INTERNAL_VAR
    int
    default 100

config ANOTHER_VISIBLE
    string "Another visible option"
    default "test"

config HIDDEN_HEX
    hex
    default 0x1000
"#;

    fs::write(&kconfig_path, kconfig_content).unwrap();

    let mut parser = Parser::new(&kconfig_path, temp_dir.path()).unwrap();
    let ast = parser.parse().unwrap();

    // Create symbol table
    let mut symbol_table = SymbolTable::new();
    symbol_table.add_symbol("VISIBLE_OPTION".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("INTERNAL_VAR".to_string(), SymbolType::U32);
    symbol_table.add_symbol("ANOTHER_VISIBLE".to_string(), SymbolType::String);
    symbol_table.add_symbol("HIDDEN_HEX".to_string(), SymbolType::Hex);

    let app = MenuConfigApp::new(ast.entries, symbol_table).unwrap();

    // Get root items and filter
    let items = app.config_state().get_items_for_path(&[]);
    let visible = app.filter_visible_items(items);

    // VISIBLE_OPTION should appear (has prompt)
    assert!(
        visible.iter().any(|item| item.id == "VISIBLE_OPTION"),
        "VISIBLE_OPTION should be visible (has prompt)"
    );

    // ANOTHER_VISIBLE should appear (has prompt)
    assert!(
        visible.iter().any(|item| item.id == "ANOTHER_VISIBLE"),
        "ANOTHER_VISIBLE should be visible (has prompt)"
    );

    // INTERNAL_VAR should NOT appear (no prompt)
    assert!(
        !visible.iter().any(|item| item.id == "INTERNAL_VAR"),
        "INTERNAL_VAR should not be visible (no prompt)"
    );

    // HIDDEN_HEX should NOT appear (no prompt)
    assert!(
        !visible.iter().any(|item| item.id == "HIDDEN_HEX"),
        "HIDDEN_HEX should not be visible (no prompt)"
    );
}

/// Test that menus and comments are always visible (they always have prompts conceptually)
#[test]
fn test_menus_and_comments_always_visible() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig_path = temp_dir.path().join("Kconfig");

    let kconfig_content = r#"
menu "Test Menu"

config OPTION_A
    bool "Option A"

comment "This is a comment"

config OPTION_B
    bool "Option B"

endmenu
"#;

    fs::write(&kconfig_path, kconfig_content).unwrap();

    let mut parser = Parser::new(&kconfig_path, temp_dir.path()).unwrap();
    let ast = parser.parse().unwrap();

    let symbol_table = SymbolTable::new();
    let app = MenuConfigApp::new(ast.entries, symbol_table).unwrap();

    // Get root items and filter
    let items = app.config_state().get_items_for_path(&[]);
    let visible = app.filter_visible_items(items);

    // Menu should be visible
    assert!(
        visible.iter().any(|item| item.label == "Test Menu"),
        "Menu should always be visible"
    );
}

/// Test combined: if condition with prompt filtering
#[test]
fn test_combined_if_and_prompt_filtering() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig_path = temp_dir.path().join("Kconfig");

    let kconfig_content = r#"
config FEATURE_ENABLED
    bool "Enable Feature"
    default y

if FEATURE_ENABLED

config VISIBLE_FEATURE_OPTION
    bool "Visible Feature Option"

config HIDDEN_INTERNAL_VAR
    int
    default 42

endif

config ALWAYS_HIDDEN
    string
    default "hidden"
"#;

    fs::write(&kconfig_path, kconfig_content).unwrap();

    let mut parser = Parser::new(&kconfig_path, temp_dir.path()).unwrap();
    let ast = parser.parse().unwrap();

    // Create symbol table with FEATURE_ENABLED=y
    let mut symbol_table = SymbolTable::new();
    symbol_table.add_symbol("FEATURE_ENABLED".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("VISIBLE_FEATURE_OPTION".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("HIDDEN_INTERNAL_VAR".to_string(), SymbolType::U32);
    symbol_table.add_symbol("ALWAYS_HIDDEN".to_string(), SymbolType::String);

    symbol_table.set_value("FEATURE_ENABLED", "y".to_string());

    let app = MenuConfigApp::new(ast.entries, symbol_table).unwrap();

    // Get root items and filter
    let items = app.config_state().get_items_for_path(&[]);
    let visible = app.filter_visible_items(items);

    // FEATURE_ENABLED should be visible (has prompt, no depends)
    assert!(
        visible.iter().any(|item| item.id == "FEATURE_ENABLED"),
        "FEATURE_ENABLED should be visible"
    );

    // VISIBLE_FEATURE_OPTION should be visible (has prompt, if condition is true)
    assert!(
        visible
            .iter()
            .any(|item| item.id == "VISIBLE_FEATURE_OPTION"),
        "VISIBLE_FEATURE_OPTION should be visible (has prompt, if true)"
    );

    // HIDDEN_INTERNAL_VAR should NOT be visible (no prompt, even though if is true)
    assert!(
        !visible.iter().any(|item| item.id == "HIDDEN_INTERNAL_VAR"),
        "HIDDEN_INTERNAL_VAR should not be visible (no prompt)"
    );

    // ALWAYS_HIDDEN should NOT be visible (no prompt)
    assert!(
        !visible.iter().any(|item| item.id == "ALWAYS_HIDDEN"),
        "ALWAYS_HIDDEN should not be visible (no prompt)"
    );
}

/// Test that choice without prompt is hidden
#[test]
fn test_choice_without_prompt_hidden() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig_path = temp_dir.path().join("Kconfig");

    let kconfig_content = r#"
choice
    prompt "Visible Choice"

config OPTION_A
    bool "Option A"

config OPTION_B
    bool "Option B"

endchoice

choice

config OPTION_C
    bool "Option C"

config OPTION_D
    bool "Option D"

endchoice
"#;

    fs::write(&kconfig_path, kconfig_content).unwrap();

    let mut parser = Parser::new(&kconfig_path, temp_dir.path()).unwrap();
    let ast = parser.parse().unwrap();

    let symbol_table = SymbolTable::new();
    let app = MenuConfigApp::new(ast.entries, symbol_table).unwrap();

    // Get root items and filter
    let items = app.config_state().get_items_for_path(&[]);
    let visible = app.filter_visible_items(items);

    // First choice with prompt should be visible
    assert!(
        visible.iter().any(|item| item.label == "Visible Choice"),
        "Choice with prompt should be visible"
    );

    // Second choice without prompt should NOT be visible
    // Note: It's a bit tricky to test this as the choice ID depends on implementation
    // We can check that we don't see "Choice" as a label (which is the default for unnamed choices)
    let unnamed_choices = visible
        .iter()
        .filter(|item| {
            matches!(item.kind, xconfig::ui::state::MenuItemKind::Choice { .. })
                && item.label == "Choice"
        })
        .count();

    assert_eq!(
        unnamed_choices, 0,
        "Choice without prompt should not be visible"
    );
}
