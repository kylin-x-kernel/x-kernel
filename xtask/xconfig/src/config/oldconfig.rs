// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{collections::HashSet, path::Path};

use crate::{
    config::ConfigReader,
    error::Result,
    kconfig::{Parser, SymbolTable},
    ui::dependency_resolver::DependencyResolver,
};

pub struct OldConfigLoader {
    kconfig_path: String,
    srctree: String,
}

pub struct ConfigChanges {
    pub new_symbols: Vec<String>,     // Symbols added in new Kconfig
    pub removed_symbols: Vec<String>, // Symbols removed from Kconfig
}

impl ConfigChanges {
    pub fn new() -> Self {
        Self {
            new_symbols: Vec::new(),
            removed_symbols: Vec::new(),
        }
    }

    pub fn has_changes(&self) -> bool {
        !self.new_symbols.is_empty() || !self.removed_symbols.is_empty()
    }

    pub fn print_summary(&self) {
        if !self.new_symbols.is_empty() {
            println!("🆕 New configuration options detected:");
            for symbol in &self.new_symbols {
                println!("  + {}", symbol);
            }
            println!();
        }

        if !self.removed_symbols.is_empty() {
            println!("⚠️  Removed configuration options (will be ignored):");
            for symbol in &self.removed_symbols {
                println!("  - {}", symbol);
            }
            println!();
        }

        if self.has_changes() {
            println!("💡 Use 'menuconfig' to review and configure new options.");
        }
    }
}

impl OldConfigLoader {
    pub fn new(kconfig_path: impl AsRef<Path>, srctree: impl AsRef<Path>) -> Self {
        Self {
            kconfig_path: kconfig_path.as_ref().to_string_lossy().to_string(),
            srctree: srctree.as_ref().to_string_lossy().to_string(),
        }
    }

    /// Load old config and merge with current Kconfig definitions
    /// Returns: (merged SymbolTable, ConfigChanges)
    pub fn load_and_merge(
        &self,
        config_path: impl AsRef<Path>,
    ) -> Result<(SymbolTable, ConfigChanges)> {
        // Parse current Kconfig to get all defined symbols
        let mut parser = Parser::new(&self.kconfig_path, &self.srctree)?;
        let ast = parser.parse()?;

        // Build symbol table from Kconfig
        let mut symbols = SymbolTable::new();

        // Extract symbols from AST
        for entry in &ast.entries {
            if let crate::kconfig::ast::Entry::Config(config) = entry {
                // Strip CONFIG_ prefix if present in symbol name
                let clean_name = config.name.strip_prefix("CONFIG_").unwrap_or(&config.name);
                symbols.add_symbol(clean_name.to_string(), config.symbol_type.clone());
            }
        }

        // Get current symbol names
        let current_symbols: HashSet<String> = symbols
            .all_symbols()
            .map(|(name, _)| name.clone())
            .collect();

        // Read old config file
        let old_config = ConfigReader::read(config_path)?;
        let old_symbol_names: HashSet<String> = old_config.keys().cloned().collect();

        // Detect differences
        let mut changes = ConfigChanges::new();

        // New symbols = current - old
        for name in &current_symbols {
            if !old_symbol_names.contains(name) {
                changes.new_symbols.push(name.clone());
                symbols.mark_as_new(name);
            }
        }

        // Removed symbols = old - current
        for name in &old_symbol_names {
            if !current_symbols.contains(name) {
                changes.removed_symbols.push(name.clone());
            }
        }

        // Apply old config values to matching symbols
        for (name, value) in old_config {
            if current_symbols.contains(&name) {
                symbols.set_value(&name, value);
                symbols.mark_from_config(&name);
            }
            // Silently ignore removed symbols
        }

        // Prune ghost values: clear any symbol whose dependency condition is not satisfied.
        // This handles the case where a defconfig contains entries like PMU_IRQ=23 that
        // "depends on ARCH_AARCH64" but the current config does not enable ARCH_AARCH64.
        let mut resolver = DependencyResolver::new();
        resolver.build_from_entries(&ast.entries);

        // Collect ghost symbol names (those with values but unsatisfied dependencies)
        // into an owned Vec first so we can mutate `symbols` afterward.
        let ghost_names: Vec<String> = symbols
            .all_symbols()
            .filter(|(name, sym)| {
                sym.value.is_some() && resolver.can_enable(name, &symbols).is_err()
            })
            .map(|(name, _)| name.clone())
            .collect();

        for name in ghost_names {
            symbols.clear_value(&name);
        }

        Ok((symbols, changes))
    }
}
