// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::path::PathBuf;

use xconfig::{
    kconfig::Parser,
    ui::state::{ConfigState, MenuItemKind},
};

/// Test that menu navigation returns the correct items for each menu
#[test]
fn test_menu_content_alignment() {
    // Setup test with the actual Kconfig from the project
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let kconfig_path = manifest_dir
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("Kconfig");
    let srctree = manifest_dir
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();

    if !kconfig_path.exists() {
        eprintln!("Kconfig not found at {:?}, skipping test", kconfig_path);
        return;
    }

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let ast = parser.parse().unwrap();

    // Build config state
    let config_state = ConfigState::build_from_entries(&ast.entries);

    // Test root menu
    let root_items = config_state.get_items_for_path(&[]);
    assert!(!root_items.is_empty(), "Root menu should have items");

    // Find Machine / Board Selection menu
    let machine_menu = root_items
        .iter()
        .find(|item| item.label == "Machine / Board Selection");
    assert!(
        machine_menu.is_some(),
        "Machine / Board Selection menu should exist in root"
    );
    let machine_menu = machine_menu.unwrap();
    assert!(
        machine_menu.has_children,
        "Machine / Board Selection should have children"
    );

    // Navigate into Machine / Board Selection
    let machine_items = config_state.get_items_for_path(&[machine_menu.id.clone()]);
    assert!(
        !machine_items.is_empty(),
        "Machine / Board Selection menu should have items"
    );

    // Check that it contains machine-related items (choice blocks or the MACHINE string config)
    let has_machine_choices = machine_items.iter().any(|item| {
        // Check for a Choice block (the per-arch machine choices) or the MACHINE string config
        matches!(item.kind, MenuItemKind::Choice { .. }) || item.id == "MACHINE"
    });
    assert!(
        has_machine_choices,
        "Machine / Board Selection should contain machine config items, but got: {:?}",
        machine_items.iter().map(|i| &i.id).collect::<Vec<_>>()
    );

    // Find Kernel Features menu
    let kernel_menu = root_items
        .iter()
        .find(|item| item.label == "Kernel Features");
    assert!(
        kernel_menu.is_some(),
        "Kernel Features menu should exist in root"
    );
    let kernel_menu = kernel_menu.unwrap();

    // Navigate into Kernel Features
    let kernel_items = config_state.get_items_for_path(&[kernel_menu.id.clone()]);
    assert!(
        !kernel_items.is_empty(),
        "Kernel Features menu should have items"
    );

    // Check that Kernel Features contains kernel-related config items
    let has_kernel_configs = kernel_items
        .iter()
        .any(|item| item.id == "NR_CPUS" || item.id == "SMP" || item.id == "FP_SIMD");
    assert!(
        has_kernel_configs,
        "Kernel Features should contain kernel config items like NR_CPUS, but got: {:?}",
        kernel_items.iter().map(|i| &i.id).collect::<Vec<_>>()
    );

    // Find Drivers Basic Configuration menu
    let drivers_menu = root_items
        .iter()
        .find(|item| item.label == "Drivers Basic Configuration");
    assert!(
        drivers_menu.is_some(),
        "Drivers Basic Configuration menu should exist in root"
    );
    let drivers_menu = drivers_menu.unwrap();

    // Navigate into Drivers Basic Configuration
    let drivers_items = config_state.get_items_for_path(&[drivers_menu.id.clone()]);
    assert!(
        !drivers_items.is_empty(),
        "Drivers Basic Configuration menu should have items"
    );

    // Check that Drivers Basic Configuration contains RTC_PADDR
    let has_rtc = drivers_items.iter().any(|item| item.id == "RTC_PADDR");
    assert!(
        has_rtc,
        "Drivers Basic Configuration should contain RTC_PADDR config, but got: {:?}",
        drivers_items.iter().map(|i| &i.id).collect::<Vec<_>>()
    );

    // Verify that menus don't have wrong content
    // Machine / Board Selection should NOT have Kernel Features content
    let machine_has_kernel_config = machine_items.iter().any(|item| item.id == "NR_CPUS");
    assert!(
        !machine_has_kernel_config,
        "Machine / Board Selection should NOT contain Kernel Features content"
    );

    // Kernel Features should NOT have RTC_PADDR
    let kernel_has_rtc = kernel_items.iter().any(|item| item.id == "RTC_PADDR");
    assert!(
        !kernel_has_rtc,
        "Kernel Features should NOT contain Drivers content"
    );
}
