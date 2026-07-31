// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{io::Write, path::Path};

use crate::{config::ConfigEngine, error::Result, kconfig::SymbolTable};

pub struct OldConfigLoader {
    kconfig_path: String,
    srctree: String,
}

pub struct ConfigChanges {
    pub new_symbols: Vec<String>,     // Symbols added in new Kconfig
    pub removed_symbols: Vec<String>, // Symbols removed from Kconfig
}

impl Default for ConfigChanges {
    fn default() -> Self {
        Self::new()
    }
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
            println!("💡 Use 'oldconfig', 'olddefconfig', or 'menuconfig' to review new options.");
        }
    }

    pub fn write_summary<W: Write>(&self, output: &mut W) -> std::io::Result<()> {
        if !self.new_symbols.is_empty() {
            writeln!(output, "🆕 New configuration options detected:")?;
            for symbol in &self.new_symbols {
                writeln!(output, "  + {}", symbol)?;
            }
            writeln!(output)?;
        }

        if !self.removed_symbols.is_empty() {
            writeln!(
                output,
                "⚠️  Removed configuration options (will be ignored):"
            )?;
            for symbol in &self.removed_symbols {
                writeln!(output, "  - {}", symbol)?;
            }
            writeln!(output)?;
        }

        if self.has_changes() {
            writeln!(
                output,
                "💡 Use 'oldconfig', 'olddefconfig', or 'menuconfig' to review new options."
            )?;
        }

        Ok(())
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
        let mut engine = ConfigEngine::from_kconfig(&self.kconfig_path, &self.srctree)?;
        let changes = engine.load_existing_config(config_path)?;
        Ok((engine.into_symbols(), changes))
    }
}
