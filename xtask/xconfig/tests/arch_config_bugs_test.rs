/// Tests for Bug 1: ARCH config value is incorrect when loading defconfig with ARCH_X86_64=y
/// Tests for Bug 2: Cross-architecture configuration pollution from defconfig
use std::fs;

use tempfile::TempDir;
use xconfig::{
    config::ConfigReader,
    kconfig::{Parser, SymbolTable},
};

/// Helper: build symbol table from entries (matching menuconfig behavior)
fn extract_symbols_from_entries(
    entries: &[xconfig::kconfig::ast::Entry],
    symbol_table: &mut SymbolTable,
) {
    use xconfig::kconfig::ast::Entry;

    for entry in entries {
        match entry {
            Entry::Config(config) => {
                symbol_table.add_symbol(config.name.clone(), config.symbol_type.clone());
                if let Some(default_value) = config.properties.evaluate_default(symbol_table) {
                    symbol_table.set_value(&config.name, default_value);
                }
            }
            Entry::MenuConfig(menuconfig) => {
                symbol_table.add_symbol(menuconfig.name.clone(), menuconfig.symbol_type.clone());
                if let Some(default_value) = menuconfig.properties.evaluate_default(symbol_table) {
                    symbol_table.set_value(&menuconfig.name, default_value);
                }
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
            Entry::Menu(menu) => extract_symbols_from_entries(&menu.entries, symbol_table),
            Entry::If(if_entry) => extract_symbols_from_entries(&if_entry.entries, symbol_table),
            _ => {}
        }
    }
}

/// Helper: collect choice groups (maps each option to its siblings)
fn collect_choice_groups(
    entries: &[xconfig::kconfig::ast::Entry],
) -> std::collections::HashMap<String, Vec<String>> {
    let mut groups = std::collections::HashMap::new();
    collect_choice_groups_inner(entries, &mut groups);
    groups
}

fn collect_choice_groups_inner(
    entries: &[xconfig::kconfig::ast::Entry],
    groups: &mut std::collections::HashMap<String, Vec<String>>,
) {
    use xconfig::kconfig::ast::Entry;
    for entry in entries {
        match entry {
            Entry::Choice(choice) => {
                let option_names: Vec<String> =
                    choice.options.iter().map(|o| o.name.clone()).collect();
                for name in &option_names {
                    groups.insert(name.clone(), option_names.clone());
                }
            }
            Entry::Menu(menu) => collect_choice_groups_inner(&menu.entries, groups),
            Entry::If(if_entry) => collect_choice_groups_inner(&if_entry.entries, groups),
            _ => {}
        }
    }
}

/// Helper: enforce choice mutual exclusion after loading .config
fn enforce_choice_mutual_exclusion(
    choice_groups: &std::collections::HashMap<String, Vec<String>>,
    symbol_table: &mut SymbolTable,
) {
    for (name, siblings) in choice_groups {
        let is_selected_from_config = symbol_table
            .get_symbol(name)
            .map(|s| s.from_config && s.value.as_deref() == Some("y"))
            .unwrap_or(false);
        if is_selected_from_config {
            for sibling in siblings {
                if sibling != name {
                    let sibling_from_config = symbol_table
                        .get_symbol(sibling)
                        .map(|s| s.from_config)
                        .unwrap_or(false);
                    if !sibling_from_config {
                        symbol_table.set_value(sibling, "n".to_string());
                    }
                }
            }
        }
    }
}

/// Helper: re-evaluate conditional defaults.
/// Derived symbols (no prompt) are ALWAYS recalculated (Linux Kconfig semantics).
/// User-editable symbols (has prompt) are only recalculated if not from_config.
fn reevaluate_defaults(entries: &[xconfig::kconfig::ast::Entry], symbol_table: &mut SymbolTable) {
    use xconfig::kconfig::ast::Entry;
    for entry in entries {
        match entry {
            Entry::Config(config) => {
                let has_conditional = config
                    .properties
                    .defaults
                    .iter()
                    .any(|d| d.condition.is_some());
                if has_conditional {
                    let is_derived = config.is_derived();
                    let from_config = symbol_table
                        .get_symbol(&config.name)
                        .map(|s| s.from_config)
                        .unwrap_or(false);
                    if is_derived || !from_config {
                        if let Some(v) = config.properties.evaluate_default(symbol_table) {
                            symbol_table.set_value(&config.name, v);
                        }
                    }
                }
            }
            Entry::Menu(menu) => reevaluate_defaults(&menu.entries, symbol_table),
            Entry::If(if_entry) => reevaluate_defaults(&if_entry.entries, symbol_table),
            _ => {}
        }
    }
}

/// Helper: collect parent if-block conditions per symbol
fn collect_symbol_if_conditions(
    entries: &[xconfig::kconfig::ast::Entry],
    parent_conditions: &[xconfig::kconfig::ast::Expr],
    result: &mut std::collections::HashMap<String, Vec<xconfig::kconfig::ast::Expr>>,
) {
    use xconfig::kconfig::ast::Entry;
    for entry in entries {
        match entry {
            Entry::Config(config) => {
                if !parent_conditions.is_empty() {
                    result.insert(config.name.clone(), parent_conditions.to_vec());
                }
            }
            Entry::MenuConfig(menuconfig) => {
                if !parent_conditions.is_empty() {
                    result.insert(menuconfig.name.clone(), parent_conditions.to_vec());
                }
            }
            Entry::Choice(choice) => {
                for option in &choice.options {
                    if !parent_conditions.is_empty() {
                        result.insert(option.name.clone(), parent_conditions.to_vec());
                    }
                }
            }
            Entry::Menu(menu) => {
                collect_symbol_if_conditions(&menu.entries, parent_conditions, result);
            }
            Entry::If(if_entry) => {
                let mut new_conditions = parent_conditions.to_vec();
                new_conditions.push(if_entry.condition.clone());
                collect_symbol_if_conditions(&if_entry.entries, &new_conditions, result);
            }
            _ => {}
        }
    }
}

/// Helper: filter symbols in if-block conditions that are not met.
/// Applies to ALL symbols (not just from_config).
fn filter_by_if_conditions(
    symbol_conditions: &std::collections::HashMap<String, Vec<xconfig::kconfig::ast::Expr>>,
    symbol_table: &mut SymbolTable,
) {
    use xconfig::kconfig::{ast::SymbolType, expr::evaluate_expr};

    for (name, conditions) in symbol_conditions {
        let all_satisfied = conditions
            .iter()
            .all(|cond| evaluate_expr(cond, symbol_table).unwrap_or(false));
        if !all_satisfied {
            if let Some(symbol) = symbol_table.get_symbol(name) {
                match symbol.symbol_type {
                    SymbolType::Bool | SymbolType::Tristate => {
                        symbol_table.set_value(name, "n".to_string());
                    }
                    _ => {
                        symbol_table.clear_value(name);
                    }
                }
            }
        }
    }
}

/// Simulate the full menuconfig loading pipeline (mirrors menuconfig_command behavior)
fn simulate_menuconfig_load(
    kconfig_path: &std::path::Path,
    srctree: &std::path::Path,
    config_path: &std::path::Path,
) -> SymbolTable {
    let mut parser = Parser::new(kconfig_path, srctree).unwrap();
    let ast = parser.parse().unwrap();

    let mut symbol_table = SymbolTable::new();
    extract_symbols_from_entries(&ast.entries, &mut symbol_table);

    if config_path.exists() {
        let config_values = ConfigReader::read(config_path).unwrap();
        for (name, value) in &config_values {
            if value == "n" {
                if let Some(symbol) = symbol_table.get_symbol(name) {
                    use xconfig::kconfig::ast::SymbolType;
                    match symbol.symbol_type {
                        SymbolType::Bool | SymbolType::Tristate => {
                            symbol_table.set_value(name, value.clone());
                            symbol_table.mark_from_config(name);
                        }
                        _ => {}
                    }
                }
            } else {
                symbol_table.set_value(name, value.clone());
                symbol_table.mark_from_config(name);
            }
        }

        let choice_groups = collect_choice_groups(&ast.entries);
        enforce_choice_mutual_exclusion(&choice_groups, &mut symbol_table);

        // Filter BEFORE reevaluate so derived symbols are computed with correct if-block values
        let mut symbol_conditions = std::collections::HashMap::new();
        collect_symbol_if_conditions(&ast.entries, &[], &mut symbol_conditions);
        filter_by_if_conditions(&symbol_conditions, &mut symbol_table);

        reevaluate_defaults(&ast.entries, &mut symbol_table);
    }

    symbol_table
}

/// Test 1: Broken defconfig with wrong ARCH= value is auto-corrected.
/// A derived symbol (no prompt) should ALWAYS be recalculated from defaults,
/// even if an incorrect value was explicitly stored in .config.
#[test]
fn test_broken_defconfig_arch_auto_corrected() {
    let kconfig_path = std::path::PathBuf::from("tests/fixtures/arch_config/Kconfig");
    let srctree = std::path::PathBuf::from("tests/fixtures/arch_config");

    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join(".config");

    // Broken .config: ARCH="aarch64" but ARCH_X86_64=y (contradictory)
    let broken_config = "\
ARCH=\"aarch64\"
ARCH_X86_64=y
PLATFORM=\"aarch64-qemu-virt\"
PLATFORM_X86_64_QEMU_VIRT=y
";
    fs::write(&config_path, broken_config).unwrap();

    let symbol_table = simulate_menuconfig_load(&kconfig_path, &srctree, &config_path);

    // ARCH is derived (no prompt): must be recalculated to "x86_64"
    assert_eq!(
        symbol_table.get_value("ARCH"),
        Some("x86_64".to_string()),
        "Broken defconfig: ARCH should be auto-corrected to 'x86_64' (derived symbol)"
    );

    // PLATFORM is derived (no prompt): must be recalculated to "x86_64-qemu-virt"
    assert_eq!(
        symbol_table.get_value("PLATFORM"),
        Some("x86_64-qemu-virt".to_string()),
        "Broken defconfig: PLATFORM should be auto-corrected to 'x86_64-qemu-virt' (derived \
         symbol)"
    );
}

/// Bug 1: When loading a defconfig with ARCH_X86_64=y, ARCH should be "x86_64" not "aarch64"
#[test]
fn test_arch_value_correct_for_x86_64_defconfig() {
    let kconfig_path = std::path::PathBuf::from("tests/fixtures/arch_config/Kconfig");
    let srctree = std::path::PathBuf::from("tests/fixtures/arch_config");

    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join(".config");

    // Minimal defconfig with only ARCH_X86_64=y (no explicit ARCH= line)
    let defconfig_content = "ARCH_X86_64=y\nPLATFORM_X86_64_QEMU_VIRT=y\n";
    fs::write(&config_path, defconfig_content).unwrap();

    let symbol_table = simulate_menuconfig_load(&kconfig_path, &srctree, &config_path);

    assert_eq!(
        symbol_table.get_value("ARCH"),
        Some("x86_64".to_string()),
        "Bug 1: ARCH should be 'x86_64' when ARCH_X86_64=y is loaded"
    );
    assert_eq!(
        symbol_table.get_value("ARCH_X86_64"),
        Some("y".to_string()),
        "ARCH_X86_64 should be set"
    );
    assert_ne!(
        symbol_table.get_value("ARCH_AARCH64"),
        Some("y".to_string()),
        "ARCH_AARCH64 should not be y when ARCH_X86_64=y"
    );
}

/// Bug 1: When loading a defconfig with ARCH_AARCH64=y, ARCH should be "aarch64"
#[test]
fn test_arch_value_correct_for_aarch64_defconfig() {
    let kconfig_path = std::path::PathBuf::from("tests/fixtures/arch_config/Kconfig");
    let srctree = std::path::PathBuf::from("tests/fixtures/arch_config");

    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join(".config");

    let defconfig_content = "ARCH_AARCH64=y\nPLATFORM_AARCH64_QEMU_VIRT=y\n";
    fs::write(&config_path, defconfig_content).unwrap();

    let symbol_table = simulate_menuconfig_load(&kconfig_path, &srctree, &config_path);

    assert_eq!(
        symbol_table.get_value("ARCH"),
        Some("aarch64".to_string()),
        "ARCH should be 'aarch64' when ARCH_AARCH64=y"
    );
}

/// Bug 2: When loading an x86_64 defconfig copied from aarch64 (with aarch64-specific configs),
/// the aarch64-specific configs should be filtered out.
#[test]
fn test_cross_arch_pollution_filtered() {
    let kconfig_path = std::path::PathBuf::from("tests/fixtures/arch_config/Kconfig");
    let srctree = std::path::PathBuf::from("tests/fixtures/arch_config");

    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join(".config");

    // x86_64 defconfig that was copied from aarch64 (has aarch64-specific entries)
    let polluted_defconfig = "\
ARCH_X86_64=y
PLATFORM_AARCH64_CROSVM_VIRT=y
PMU_IRQ=23
PSCI_METHOD=hvc
";
    fs::write(&config_path, polluted_defconfig).unwrap();

    let symbol_table = simulate_menuconfig_load(&kconfig_path, &srctree, &config_path);

    // ARCH_X86_64 should be set
    assert_eq!(
        symbol_table.get_value("ARCH_X86_64"),
        Some("y".to_string()),
        "ARCH_X86_64 should be set"
    );

    // aarch64-specific platform should be filtered out (set to "n")
    assert_ne!(
        symbol_table.get_value("PLATFORM_AARCH64_CROSVM_VIRT"),
        Some("y".to_string()),
        "Bug 2: PLATFORM_AARCH64_CROSVM_VIRT should be filtered when ARCH_X86_64=y"
    );

    // aarch64-specific int config should be cleared
    assert_eq!(
        symbol_table.get_value("PMU_IRQ"),
        None,
        "Bug 2: PMU_IRQ (aarch64-specific) should be cleared when ARCH_X86_64=y"
    );

    // aarch64-specific string config should be cleared
    assert_eq!(
        symbol_table.get_value("PSCI_METHOD"),
        None,
        "Bug 2: PSCI_METHOD (aarch64-specific) should be cleared when ARCH_X86_64=y"
    );
}

/// Verify that aarch64-specific configs are preserved when ARCH_AARCH64=y
#[test]
fn test_aarch64_configs_preserved_for_aarch64() {
    let kconfig_path = std::path::PathBuf::from("tests/fixtures/arch_config/Kconfig");
    let srctree = std::path::PathBuf::from("tests/fixtures/arch_config");

    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join(".config");

    let aarch64_defconfig = "\
ARCH_AARCH64=y
PLATFORM_AARCH64_QEMU_VIRT=y
PMU_IRQ=23
PSCI_METHOD=hvc
";
    fs::write(&config_path, aarch64_defconfig).unwrap();

    let symbol_table = simulate_menuconfig_load(&kconfig_path, &srctree, &config_path);

    assert_eq!(
        symbol_table.get_value("ARCH_AARCH64"),
        Some("y".to_string()),
        "ARCH_AARCH64 should be set"
    );
    assert_eq!(
        symbol_table.get_value("PLATFORM_AARCH64_QEMU_VIRT"),
        Some("y".to_string()),
        "PLATFORM_AARCH64_QEMU_VIRT should be preserved for aarch64"
    );
    assert_eq!(
        symbol_table.get_value("PMU_IRQ"),
        Some("23".to_string()),
        "PMU_IRQ should be preserved for aarch64"
    );
    assert_eq!(
        symbol_table.get_value("PSCI_METHOD"),
        Some("hvc".to_string()),
        "PSCI_METHOD should be preserved for aarch64"
    );
}
