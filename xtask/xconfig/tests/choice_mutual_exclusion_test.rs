use std::io::Write;
use tempfile::NamedTempFile;
use xconfig::kconfig::Parser;
use xconfig::ui::state::ConfigState;

/// Test that choice options have parent_choice field set correctly
#[test]
fn test_choice_parent_child_relationship() {
    // Create a temporary Kconfig file with a choice
    let kconfig_content = r#"
mainmenu "Test Configuration"

choice
    prompt "Target Architecture"
    default ARCH_ARM64

config ARCH_ARM64
    bool "ARM 64-bit"

config ARCH_X86_64
    bool "x86_64"

config ARCH_RISCV64
    bool "RISC-V 64-bit"

endchoice
"#;

    let mut temp_file = NamedTempFile::new().unwrap();
    write!(temp_file, "{}", kconfig_content).unwrap();
    let kconfig_path = temp_file.path().to_path_buf();
    let srctree = kconfig_path.parent().unwrap();

    // Parse the Kconfig
    let mut parser = Parser::new(&kconfig_path, &srctree.to_path_buf()).unwrap();
    let ast = parser.parse().unwrap();

    // Build ConfigState
    let config_state = ConfigState::build_from_entries(&ast.entries);

    // Find choice options
    let arch_arm64 = config_state
        .all_items
        .iter()
        .find(|item| item.id == "ARCH_ARM64")
        .expect("ARCH_ARM64 should exist");
    let arch_x86_64 = config_state
        .all_items
        .iter()
        .find(|item| item.id == "ARCH_X86_64")
        .expect("ARCH_X86_64 should exist");
    let arch_riscv64 = config_state
        .all_items
        .iter()
        .find(|item| item.id == "ARCH_RISCV64")
        .expect("ARCH_RISCV64 should exist");

    // All choice options should have parent_choice set
    assert!(
        arch_arm64.parent_choice.is_some(),
        "ARCH_ARM64 should have parent_choice set"
    );
    assert!(
        arch_x86_64.parent_choice.is_some(),
        "ARCH_X86_64 should have parent_choice set"
    );
    assert!(
        arch_riscv64.parent_choice.is_some(),
        "ARCH_RISCV64 should have parent_choice set"
    );

    // All should point to the same choice
    let choice_id = arch_arm64.parent_choice.as_ref().unwrap();
    assert_eq!(arch_x86_64.parent_choice.as_ref().unwrap(), choice_id);
    assert_eq!(arch_riscv64.parent_choice.as_ref().unwrap(), choice_id);
}

/// Test that normal config items (not in a choice) don't have parent_choice set
#[test]
fn test_non_choice_config_has_no_parent() {
    let kconfig_content = r#"
mainmenu "Test Configuration"

config STANDALONE_OPTION
    bool "A standalone option"

choice
    prompt "Select One"
    
config CHOICE_OPTION_A
    bool "Option A"
    
config CHOICE_OPTION_B
    bool "Option B"
    
endchoice
"#;

    let mut temp_file = NamedTempFile::new().unwrap();
    write!(temp_file, "{}", kconfig_content).unwrap();
    let kconfig_path = temp_file.path().to_path_buf();
    let srctree = kconfig_path.parent().unwrap();

    let mut parser = Parser::new(&kconfig_path, &srctree.to_path_buf()).unwrap();
    let ast = parser.parse().unwrap();

    let config_state = ConfigState::build_from_entries(&ast.entries);

    // Find standalone option
    let standalone = config_state
        .all_items
        .iter()
        .find(|item| item.id == "STANDALONE_OPTION")
        .expect("STANDALONE_OPTION should exist");

    // Standalone option should NOT have parent_choice
    assert!(
        standalone.parent_choice.is_none(),
        "Standalone config should not have parent_choice"
    );

    // Find choice options
    let option_a = config_state
        .all_items
        .iter()
        .find(|item| item.id == "CHOICE_OPTION_A")
        .expect("CHOICE_OPTION_A should exist");
    let option_b = config_state
        .all_items
        .iter()
        .find(|item| item.id == "CHOICE_OPTION_B")
        .expect("CHOICE_OPTION_B should exist");

    // Choice options should have parent_choice
    assert!(
        option_a.parent_choice.is_some(),
        "CHOICE_OPTION_A should have parent_choice"
    );
    assert!(
        option_b.parent_choice.is_some(),
        "CHOICE_OPTION_B should have parent_choice"
    );
}

/// Test that multiple separate choices have different parent_choice values
#[test]
fn test_multiple_choices_have_different_parents() {
    let kconfig_content = r#"
mainmenu "Test Configuration"

choice
    prompt "First Choice"
    
config FIRST_A
    bool "First A"
    
config FIRST_B
    bool "First B"
    
endchoice

choice
    prompt "Second Choice"
    
config SECOND_A
    bool "Second A"
    
config SECOND_B
    bool "Second B"
    
endchoice
"#;

    let mut temp_file = NamedTempFile::new().unwrap();
    write!(temp_file, "{}", kconfig_content).unwrap();
    let kconfig_path = temp_file.path().to_path_buf();
    let srctree = kconfig_path.parent().unwrap();

    let mut parser = Parser::new(&kconfig_path, &srctree.to_path_buf()).unwrap();
    let ast = parser.parse().unwrap();

    let config_state = ConfigState::build_from_entries(&ast.entries);

    // Find options from first choice
    let first_a = config_state
        .all_items
        .iter()
        .find(|item| item.id == "FIRST_A")
        .expect("FIRST_A should exist");
    let first_b = config_state
        .all_items
        .iter()
        .find(|item| item.id == "FIRST_B")
        .expect("FIRST_B should exist");

    // Find options from second choice
    let second_a = config_state
        .all_items
        .iter()
        .find(|item| item.id == "SECOND_A")
        .expect("SECOND_A should exist");
    let second_b = config_state
        .all_items
        .iter()
        .find(|item| item.id == "SECOND_B")
        .expect("SECOND_B should exist");

    // Get choice IDs
    let first_choice_id = first_a.parent_choice.as_ref().unwrap();
    let second_choice_id = second_a.parent_choice.as_ref().unwrap();

    // First choice options should have the same parent
    assert_eq!(first_b.parent_choice.as_ref().unwrap(), first_choice_id);

    // Second choice options should have the same parent
    assert_eq!(second_b.parent_choice.as_ref().unwrap(), second_choice_id);

    // First and second choices should have different parents
    assert_ne!(
        first_choice_id, second_choice_id,
        "Different choices should have different parent IDs"
    );
}
