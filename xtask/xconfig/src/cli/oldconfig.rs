use crate::config::{ConfigWriter, OldConfigLoader};
use crate::error::Result;
use std::path::PathBuf;

pub fn oldconfig_command(
    config: PathBuf,
    kconfig: PathBuf,
    srctree: PathBuf,
    auto_defaults: bool,
) -> Result<()> {
    println!("Loading existing configuration...");
    println!("Config: {}", config.display());
    println!("Kconfig: {}", kconfig.display());

    // Load and merge old config with current Kconfig
    let loader = OldConfigLoader::new(&kconfig, &srctree);
    let (mut symbols, changes) = loader.load_and_merge(&config)?;

    // Print summary of changes
    if changes.has_changes() {
        println!();
        changes.print_summary();
    } else {
        println!("✅ No configuration changes detected.");
    }

    // Apply default values to new symbols if requested
    if auto_defaults {
        println!("\nApplying default values to new symbols...");
        // Collect names first to avoid borrow checker issues
        let new_symbol_names: Vec<String> = symbols
            .get_new_symbols()
            .iter()
            .map(|s| s.name.clone())
            .collect();

        for name in new_symbol_names {
            if let Some(symbol) = symbols.get_symbol(&name) {
                if symbol.value.is_none() {
                    let symbol_type = symbol.symbol_type.clone();
                    match symbol_type {
                        crate::kconfig::ast::SymbolType::Bool
                        | crate::kconfig::ast::SymbolType::Tristate => {
                            symbols.set_value(&name, "n".to_string());
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    // Save updated configuration
    println!("\nSaving configuration to {}...", config.display());
    ConfigWriter::write(&config, &symbols)?;
    println!("✅ Configuration saved successfully.");

    Ok(())
}
