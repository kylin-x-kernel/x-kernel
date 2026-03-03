// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{io, path::PathBuf};

use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::{
    config::ConfigReader,
    error::Result,
    kconfig::Parser,
    ui::{MenuConfigApp, dependency_resolver::DependencyResolver},
};

pub fn menuconfig_command(kconfig: PathBuf, srctree: PathBuf) -> Result<()> {
    println!("Loading configuration...");

    // Parse Kconfig
    let mut parser = Parser::new(&kconfig, &srctree)?;
    let ast = parser.parse()?;

    println!("Parsed {} entries", ast.entries.len());

    // Load existing config if present
    let mut symbol_table = crate::kconfig::SymbolTable::new();

    // Extract all symbols from AST
    extract_symbols_from_entries(&ast.entries, &mut symbol_table);

    // Load existing .config if it exists
    if std::path::Path::new(".config").exists() {
        println!("Loading existing .config...");
        let config_values = ConfigReader::read(".config")?;
        for (name, value) in &config_values {
            // For symbols with types other than bool/tristate, don't override defaults
            // with "n" (from "# XXX is not set" comments) as "n" is not meaningful
            // for hex/int/string types. This preserves defaults for new symbols.
            if value == "n" {
                // Check if this symbol is bool or tristate
                if let Some(symbol) = symbol_table.get_symbol(name) {
                    use crate::kconfig::ast::SymbolType;
                    match symbol.symbol_type {
                        SymbolType::Bool | SymbolType::Tristate => {
                            // For bool/tristate, "n" is valid, apply it
                            symbol_table.set_value(name, value.clone());
                            symbol_table.mark_from_config(name);
                        }
                        _ => {
                            // For hex/int/string/range, skip "n" values
                            // This preserves the default value
                        }
                    }
                } else {
                    // Symbol not found, skip
                }
            } else {
                // For non-"n" values, always apply from config
                symbol_table.set_value(name, value.clone());
                symbol_table.mark_from_config(name);
            }
        }

        // Enforce choice mutual exclusion: when a choice option is loaded as "y",
        // set all other options in the same choice group to "n" (Fix Bug 2 prerequisite)
        let choice_groups = collect_choice_groups(&ast.entries);
        enforce_choice_mutual_exclusion(&choice_groups, &mut symbol_table);

        // Filter out symbols in if-blocks whose conditions are not met.
        // This runs BEFORE reevaluate_defaults so that derived symbols are recalculated
        // with accurate if-block symbol values (e.g. PLATFORM_AARCH64_* cleared before PLATFORM is recalculated).
        let mut symbol_conditions = std::collections::HashMap::new();
        collect_symbol_if_conditions(&ast.entries, &[], &mut symbol_conditions);
        filter_by_if_conditions(&symbol_conditions, &mut symbol_table);

        // Re-evaluate conditional defaults (Fix Bug 1).
        // Derived symbols (no prompt) are ALWAYS recalculated (Linux Kconfig semantics).
        reevaluate_defaults(&ast.entries, &mut symbol_table);

        // Filter ghost configs: clear symbols whose dependencies are not satisfied.
        // This handles cases like PMU_IRQ=23 in x86_64 defconfig where PMU_IRQ depends on ARCH_AARCH64.
        let mut dep_resolver = DependencyResolver::new();
        dep_resolver.build_from_entries(&ast.entries);

        let ghost_names: Vec<String> = symbol_table
            .all_symbols()
            .filter(|(name, sym)| {
                // A symbol is a "ghost" if it has a value but dependencies are not met
                sym.value.is_some() && dep_resolver.can_enable(name, &symbol_table).is_err()
            })
            .map(|(name, _)| name.clone())
            .collect();

        if !ghost_names.is_empty() {
            println!(
                "🧹 Clearing {} ghost configuration(s) with unsatisfied dependencies:",
                ghost_names.len()
            );
            for name in &ghost_names {
                println!("  - {}", name);
            }
        }

        for name in ghost_names {
            symbol_table.clear_value(&name);
        }
    } else {
        println!("No existing .config found, using defaults");
    }

    println!("Launching TUI...");

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create and run app
    let mut app = MenuConfigApp::new(ast.entries, symbol_table)?;
    let res = app.run(&mut terminal);

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    res
}

fn extract_symbols_from_entries(
    entries: &[crate::kconfig::ast::Entry],
    symbol_table: &mut crate::kconfig::SymbolTable,
) {
    use crate::kconfig::ast::Entry;

    for entry in entries {
        match entry {
            Entry::Config(config) => {
                symbol_table.add_symbol(config.name.clone(), config.symbol_type.clone());

                // Use the new evaluate_default method
                if let Some(default_value) = config.properties.evaluate_default(symbol_table) {
                    symbol_table.set_value(&config.name, default_value);
                }
            }
            Entry::MenuConfig(menuconfig) => {
                symbol_table.add_symbol(menuconfig.name.clone(), menuconfig.symbol_type.clone());

                // Also evaluate defaults for menuconfig
                if let Some(default_value) = menuconfig.properties.evaluate_default(symbol_table) {
                    symbol_table.set_value(&menuconfig.name, default_value);
                }
            }
            Entry::Choice(choice) => {
                for option in &choice.options {
                    symbol_table.add_symbol(option.name.clone(), option.symbol_type.clone());
                }

                // Apply choice default if specified
                if let Some(default_name) = &choice.default {
                    symbol_table.set_value(default_name, "y".to_string());
                } else if let Some(first_option) = choice.options.first() {
                    // No default specified, select first option (standard Kconfig behavior)
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

/// Collect choice groups: maps each choice option name to all sibling option names.
fn collect_choice_groups(
    entries: &[crate::kconfig::ast::Entry],
) -> std::collections::HashMap<String, Vec<String>> {
    let mut groups: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    collect_choice_groups_inner(entries, &mut groups);
    groups
}

fn collect_choice_groups_inner(
    entries: &[crate::kconfig::ast::Entry],
    groups: &mut std::collections::HashMap<String, Vec<String>>,
) {
    use crate::kconfig::ast::Entry;

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

/// Enforce choice mutual exclusion: when a choice option loaded from .config is "y",
/// set all other options in the same choice group to "n" (if not explicitly from .config).
fn enforce_choice_mutual_exclusion(
    choice_groups: &std::collections::HashMap<String, Vec<String>>,
    symbol_table: &mut crate::kconfig::SymbolTable,
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

/// Re-evaluate conditional defaults.
/// Derived symbols (no prompt) are ALWAYS recalculated from defaults (Linux Kconfig semantics).
/// User-editable symbols (has prompt) are only recalculated if not loaded from .config.
fn reevaluate_defaults(
    entries: &[crate::kconfig::ast::Entry],
    symbol_table: &mut crate::kconfig::SymbolTable,
) {
    use crate::kconfig::ast::Entry;

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
                        if let Some(default_value) =
                            config.properties.evaluate_default(symbol_table)
                        {
                            symbol_table.set_value(&config.name, default_value);
                        }
                    }
                }
            }
            Entry::MenuConfig(menuconfig) => {
                let has_conditional = menuconfig
                    .properties
                    .defaults
                    .iter()
                    .any(|d| d.condition.is_some());
                if has_conditional {
                    let is_derived = menuconfig.is_derived();
                    let from_config = symbol_table
                        .get_symbol(&menuconfig.name)
                        .map(|s| s.from_config)
                        .unwrap_or(false);
                    if is_derived || !from_config {
                        if let Some(default_value) =
                            menuconfig.properties.evaluate_default(symbol_table)
                        {
                            symbol_table.set_value(&menuconfig.name, default_value);
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

/// Collect parent if-block conditions for each symbol in the AST.
fn collect_symbol_if_conditions(
    entries: &[crate::kconfig::ast::Entry],
    parent_conditions: &[crate::kconfig::ast::Expr],
    result: &mut std::collections::HashMap<String, Vec<crate::kconfig::ast::Expr>>,
) {
    use crate::kconfig::ast::Entry;

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

/// Filter out symbols in if-block conditions that are not met.
/// Applies to ALL symbols (not just from_config) so that derived symbols are
/// recalculated with accurate values from if-guarded choice defaults.
fn filter_by_if_conditions(
    symbol_conditions: &std::collections::HashMap<String, Vec<crate::kconfig::ast::Expr>>,
    symbol_table: &mut crate::kconfig::SymbolTable,
) {
    use crate::kconfig::{ast::SymbolType, expr::evaluate_expr};

    for (name, conditions) in symbol_conditions {
        let all_satisfied = conditions
            .iter()
            .all(|cond| evaluate_expr(cond, symbol_table).unwrap_or(false));

        if !all_satisfied {
            if let Some(symbol) = symbol_table.get_symbol(name) {
                if symbol.from_config {
                    eprintln!(
                        "Filtering config {} (parent conditions not satisfied)",
                        name
                    );
                }
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
