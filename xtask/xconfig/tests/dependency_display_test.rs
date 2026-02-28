use std::path::PathBuf;
use xconfig::kconfig::Parser;
use xconfig::ui::state::ConfigState;

#[test]
fn test_dependency_tracking() {
    // Parse the sample project Kconfig
    let kconfig_path = PathBuf::from("examples/sample_project/Kconfig");
    let srctree = PathBuf::from("examples/sample_project");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let ast = parser.parse().unwrap();

    // Build ConfigState with dependency mappings
    let config_state = ConfigState::build_from_entries(&ast.entries);

    // Test 1: Check that PREEMPT has selected_by list
    let preempt_item = config_state
        .all_items
        .iter()
        .find(|item| item.id == "PREEMPT")
        .expect("PREEMPT config should exist");

    assert!(
        preempt_item
            .selected_by
            .contains(&"SCHEDULER_RT".to_string()),
        "PREEMPT should be selected by SCHEDULER_RT, got: {:?}",
        preempt_item.selected_by
    );

    // Test 2: Check that SCHEDULER_RT has selects list
    let scheduler_item = config_state
        .all_items
        .iter()
        .find(|item| item.id == "SCHEDULER_RT")
        .expect("SCHEDULER_RT config should exist");

    assert!(
        scheduler_item.selects.contains(&"PREEMPT".to_string()),
        "SCHEDULER_RT should select PREEMPT, got: {:?}",
        scheduler_item.selects
    );

    // Test 3: Check that SCHEDULER_RT has depends_on
    assert!(
        scheduler_item.depends_on.is_some(),
        "SCHEDULER_RT should have depends_on"
    );

    // Test 4: Check that ADVANCED_FEATURES has implied_by list
    let advanced_item = config_state
        .all_items
        .iter()
        .find(|item| item.id == "ADVANCED_FEATURES")
        .expect("ADVANCED_FEATURES config should exist");

    assert!(
        advanced_item.implied_by.contains(&"PROFILING".to_string()),
        "ADVANCED_FEATURES should be implied by PROFILING, got: {:?}",
        advanced_item.implied_by
    );

    // Test 5: Check that PROFILING has implies list
    let profiling_item = config_state
        .all_items
        .iter()
        .find(|item| item.id == "PROFILING")
        .expect("PROFILING config should exist");

    assert!(
        profiling_item
            .implies
            .contains(&"ADVANCED_FEATURES".to_string()),
        "PROFILING should imply ADVANCED_FEATURES, got: {:?}",
        profiling_item.implies
    );

    println!("✓ All dependency tracking tests passed!");
    println!("  - PREEMPT is selected by: {:?}", preempt_item.selected_by);
    println!("  - SCHEDULER_RT selects: {:?}", scheduler_item.selects);
    println!(
        "  - ADVANCED_FEATURES is implied by: {:?}",
        advanced_item.implied_by
    );
    println!("  - PROFILING implies: {:?}", profiling_item.implies);
}

#[test]
fn test_menu_tree_dependencies() {
    // Parse the sample project Kconfig
    let kconfig_path = PathBuf::from("examples/sample_project/Kconfig");
    let srctree = PathBuf::from("examples/sample_project");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let ast = parser.parse().unwrap();

    // Build ConfigState with dependency mappings
    let config_state = ConfigState::build_from_entries(&ast.entries);

    // Get items from the kernel menu
    let menu_id = "menu_Kernel_Configuration";
    let kernel_items = config_state.menu_tree.get(menu_id);

    assert!(
        kernel_items.is_some(),
        "Kernel Configuration menu should exist"
    );

    let items = kernel_items.unwrap();

    // Find PREEMPT in the menu tree
    let preempt_in_menu = items.iter().find(|item| item.id == "PREEMPT");

    if let Some(preempt) = preempt_in_menu {
        assert!(
            preempt.selected_by.contains(&"SCHEDULER_RT".to_string()),
            "PREEMPT in menu_tree should also have selected_by populated"
        );
        println!("✓ Menu tree dependencies are properly populated");
    }
}
