use std::path::PathBuf;

use crate::{
    config::{ConfigGenerator, ConfigWriter},
    error::Result,
    kconfig::Parser,
    ui::dependency_resolver::DependencyResolver,
};

pub fn saveconfig_command(output: PathBuf, kconfig: PathBuf, srctree: PathBuf) -> Result<()> {
    println!("Saving configuration...");
    println!("Kconfig: {}", kconfig.display());
    println!("Output: {}", output.display());

    // Parse Kconfig to get symbol definitions
    let mut parser = Parser::new(&kconfig, &srctree)?;
    let ast = parser.parse()?;

    // Build symbol table from Kconfig
    let mut symbols = crate::kconfig::SymbolTable::new();

    // Extract symbols from AST and apply defaults
    extract_symbols_from_entries(&ast.entries, &mut symbols);

    // Build dependency resolver so we can skip inactive symbols
    let mut dep_resolver = DependencyResolver::new();
    dep_resolver.build_from_entries(&ast.entries);

    // Collect symbols whose dependencies are not met; clear their values so the
    // writer treats them as "not set" (bool/tristate) or skips them (other types).
    let inactive: Vec<String> = symbols
        .all_symbols()
        .filter_map(|(name, _)| {
            if dep_resolver.can_enable(name, &symbols).is_err() {
                Some(name.clone())
            } else {
                None
            }
        })
        .collect();

    for name in &inactive {
        symbols.clear_value(name);
    }

    // Write .config file
    ConfigWriter::write(&output, &symbols)?;
    println!("✅ Saved .config to {}", output.display());

    // Generate auto.conf
    let auto_conf = output
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("auto.conf");
    ConfigGenerator::generate_auto_conf(&auto_conf, &symbols)?;
    println!("✅ Generated {}", auto_conf.display());

    // Generate autoconf.h
    let autoconf_h = output
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("autoconf.h");
    ConfigGenerator::generate_autoconf_h(&autoconf_h, &symbols)?;
    println!("✅ Generated {}", autoconf_h.display());

    Ok(())
}

fn extract_symbols_from_entries(
    entries: &[crate::kconfig::ast::Entry],
    symbols: &mut crate::kconfig::SymbolTable,
) {
    extract_symbols_internal(entries, symbols, true);
}

fn extract_symbols_internal(
    entries: &[crate::kconfig::ast::Entry],
    symbols: &mut crate::kconfig::SymbolTable,
    apply_defaults: bool,
) {
    use crate::kconfig::{ast::Entry, expr::evaluate_expr};

    for entry in entries {
        match entry {
            Entry::Config(config) => {
                // Strip CONFIG_ prefix if present
                let clean_name = config.name.strip_prefix("CONFIG_").unwrap_or(&config.name);
                symbols.add_symbol(clean_name.to_string(), config.symbol_type.clone());

                if apply_defaults {
                    // Use the new evaluate_default method
                    if let Some(default_value) = config.properties.evaluate_default(symbols) {
                        symbols.set_value(clean_name, default_value);
                    } else {
                        // If no default was applied, set to 'n' for bool/tristate
                        match config.symbol_type {
                            crate::kconfig::ast::SymbolType::Bool
                            | crate::kconfig::ast::SymbolType::Tristate => {
                                symbols.set_value(clean_name, "n".to_string());
                            }
                            _ => {}
                        }
                    }
                }
            }
            Entry::MenuConfig(menuconfig) => {
                let clean_name = menuconfig
                    .name
                    .strip_prefix("CONFIG_")
                    .unwrap_or(&menuconfig.name);
                symbols.add_symbol(clean_name.to_string(), menuconfig.symbol_type.clone());

                if apply_defaults {
                    // Also evaluate defaults for menuconfig
                    if let Some(default_value) = menuconfig.properties.evaluate_default(symbols) {
                        symbols.set_value(clean_name, default_value);
                    } else {
                        // If no default was applied, set to 'n' for bool/tristate
                        match menuconfig.symbol_type {
                            crate::kconfig::ast::SymbolType::Bool
                            | crate::kconfig::ast::SymbolType::Tristate => {
                                symbols.set_value(clean_name, "n".to_string());
                            }
                            _ => {}
                        }
                    }
                }
            }
            Entry::Choice(choice) => {
                for option in &choice.options {
                    let clean_name = option.name.strip_prefix("CONFIG_").unwrap_or(&option.name);
                    symbols.add_symbol(clean_name.to_string(), option.symbol_type.clone());
                }

                if apply_defaults {
                    // Apply choice default if specified
                    if let Some(default_name) = &choice.default {
                        let clean_default =
                            default_name.strip_prefix("CONFIG_").unwrap_or(default_name);
                        symbols.set_value(clean_default, "y".to_string());
                    } else if let Some(first_option) = choice.options.first() {
                        // No default specified, select first option (standard Kconfig behavior)
                        let clean_name = first_option
                            .name
                            .strip_prefix("CONFIG_")
                            .unwrap_or(&first_option.name);
                        symbols.set_value(clean_name, "y".to_string());
                    }
                }
            }
            Entry::Menu(menu) => {
                extract_symbols_internal(&menu.entries, symbols, apply_defaults);
            }
            Entry::If(if_entry) => {
                // Only apply defaults inside this if-block when its condition is satisfied.
                // Symbols are always registered regardless of condition.
                let condition_met =
                    apply_defaults && evaluate_expr(&if_entry.condition, symbols).unwrap_or(false);
                extract_symbols_internal(&if_entry.entries, symbols, condition_met);
            }
            _ => {}
        }
    }
}
