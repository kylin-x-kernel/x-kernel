use std::fs;
use tempfile::TempDir;
use xconfig::kconfig::Parser;
use xconfig::ui::state::ConfigState;

/// Test that entries inside `if` blocks are properly processed and displayed in menu tree
#[test]
fn test_if_block_choice_visibility() {
    // Create a temporary directory for test files
    let temp_dir = TempDir::new().unwrap();
    let kconfig_path = temp_dir.path().join("Kconfig");

    // Create a test Kconfig with choice inside if block (similar to platform selection)
    let kconfig_content = r#"
mainmenu "Test Config"

choice
    prompt "Target Architecture"
    default ARCH_AARCH64

config ARCH_AARCH64
    bool "AArch64"

config ARCH_RISCV64
    bool "RISC-V 64-bit"

endchoice

menu "Platform Selection"

if ARCH_AARCH64

choice
    prompt "AArch64 Platform"
    default PLATFORM_QEMU

config PLATFORM_QEMU
    bool "QEMU virt machine"

config PLATFORM_CROSVM
    bool "Crosvm virt machine"

endchoice

endif

if ARCH_RISCV64

config PLATFORM_RISCV_QEMU
    bool "RISC-V QEMU"

endif

endmenu
"#;

    fs::write(&kconfig_path, kconfig_content).unwrap();

    // Parse the Kconfig
    let mut parser = Parser::new(&kconfig_path, temp_dir.path()).unwrap();
    let ast = parser.parse().unwrap();

    // Build ConfigState
    let config_state = ConfigState::build_from_entries(&ast.entries);

    // Verify that all items are collected (including those in if blocks)
    let all_item_ids: Vec<String> = config_state
        .all_items
        .iter()
        .map(|i| i.id.clone())
        .collect();

    // Should contain the architecture choice and its options (choice ID is based on first option)
    assert!(
        all_item_ids.contains(&"choice_ARCH_AARCH64".to_string()),
        "Should contain architecture choice"
    );
    assert!(
        all_item_ids.contains(&"ARCH_AARCH64".to_string()),
        "Should contain ARCH_AARCH64"
    );
    assert!(
        all_item_ids.contains(&"ARCH_RISCV64".to_string()),
        "Should contain ARCH_RISCV64"
    );

    // Should contain the menu
    assert!(
        all_item_ids.contains(&"menu_Platform_Selection".to_string()),
        "Should contain Platform Selection menu"
    );

    // CRITICAL: Should contain the platform choice that's inside the if block
    // We check for the specific platform configs which are unique to the if block
    assert!(
        all_item_ids.contains(&"PLATFORM_QEMU".to_string()),
        "Should contain PLATFORM_QEMU from inside if ARCH_AARCH64 block"
    );
    assert!(
        all_item_ids.contains(&"PLATFORM_CROSVM".to_string()),
        "Should contain PLATFORM_CROSVM from inside if ARCH_AARCH64 block"
    );

    // Should contain platform configs from inside other if blocks
    assert!(
        all_item_ids.contains(&"PLATFORM_RISCV_QEMU".to_string()),
        "Should contain PLATFORM_RISCV_QEMU from inside if ARCH_RISCV64 block"
    );

    // Verify menu tree structure - the Platform Selection menu should contain items from if blocks
    let platform_menu_items = config_state.menu_tree.get("menu_Platform_Selection");
    assert!(
        platform_menu_items.is_some(),
        "Platform Selection menu should exist in menu_tree"
    );

    let platform_items = platform_menu_items.unwrap();
    let platform_item_ids: Vec<String> = platform_items.iter().map(|i| i.id.clone()).collect();

    // The menu should contain the platform-specific configs from inside the if blocks
    assert!(
        platform_item_ids.contains(&"PLATFORM_QEMU".to_string()),
        "Platform Selection menu should contain PLATFORM_QEMU from if ARCH_AARCH64 block. Found: {:?}",
        platform_item_ids
    );
    assert!(
        platform_item_ids.contains(&"PLATFORM_CROSVM".to_string()),
        "Platform Selection menu should contain PLATFORM_CROSVM from if ARCH_AARCH64 block. Found: {:?}",
        platform_item_ids
    );
    assert!(
        platform_item_ids.contains(&"PLATFORM_RISCV_QEMU".to_string()),
        "Platform Selection menu should contain PLATFORM_RISCV_QEMU from if ARCH_RISCV64 block. Found: {:?}",
        platform_item_ids
    );
}

/// Test that nested menus inside if blocks work correctly
#[test]
fn test_nested_menu_in_if_block() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig_path = temp_dir.path().join("Kconfig");

    let kconfig_content = r#"
config FEATURE_A
    bool "Enable Feature A"

if FEATURE_A

menu "Feature A Options"

config OPTION_1
    bool "Option 1"

config OPTION_2
    bool "Option 2"

endmenu

endif
"#;

    fs::write(&kconfig_path, kconfig_content).unwrap();

    let mut parser = Parser::new(&kconfig_path, temp_dir.path()).unwrap();
    let ast = parser.parse().unwrap();

    let config_state = ConfigState::build_from_entries(&ast.entries);

    // Verify the menu inside if block is processed
    let all_item_ids: Vec<String> = config_state
        .all_items
        .iter()
        .map(|i| i.id.clone())
        .collect();

    assert!(
        all_item_ids.contains(&"FEATURE_A".to_string()),
        "Should contain FEATURE_A"
    );
    assert!(
        all_item_ids.contains(&"menu_Feature_A_Options".to_string()),
        "Should contain menu from inside if block"
    );
    assert!(
        all_item_ids.contains(&"OPTION_1".to_string()),
        "Should contain OPTION_1 from menu inside if block"
    );
    assert!(
        all_item_ids.contains(&"OPTION_2".to_string()),
        "Should contain OPTION_2 from menu inside if block"
    );
}

/// Test that multiple if blocks at the same level are all processed
#[test]
fn test_multiple_if_blocks() {
    let temp_dir = TempDir::new().unwrap();
    let kconfig_path = temp_dir.path().join("Kconfig");

    let kconfig_content = r#"
menu "Options"

if ARCH_A
config OPTION_A1
    bool "Option A1"
endif

if ARCH_B
config OPTION_B1
    bool "Option B1"
endif

if ARCH_C
config OPTION_C1
    bool "Option C1"
endif

endmenu
"#;

    fs::write(&kconfig_path, kconfig_content).unwrap();

    let mut parser = Parser::new(&kconfig_path, temp_dir.path()).unwrap();
    let ast = parser.parse().unwrap();

    let config_state = ConfigState::build_from_entries(&ast.entries);

    // All options from different if blocks should be in the menu
    let menu_items = config_state.menu_tree.get("menu_Options").unwrap();
    let item_ids: Vec<String> = menu_items.iter().map(|i| i.id.clone()).collect();

    assert!(
        item_ids.contains(&"OPTION_A1".to_string()),
        "Should contain OPTION_A1 from first if block"
    );
    assert!(
        item_ids.contains(&"OPTION_B1".to_string()),
        "Should contain OPTION_B1 from second if block"
    );
    assert!(
        item_ids.contains(&"OPTION_C1".to_string()),
        "Should contain OPTION_C1 from third if block"
    );

    // All should be at the same depth (within the menu)
    assert_eq!(menu_items.len(), 3, "Should have 3 items in the menu");
}
