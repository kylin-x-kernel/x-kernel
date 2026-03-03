// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use crate::{
    config::ConfigGenerator,
    error::Result,
    kconfig::{
        Parser,
        ast::{Entry, SymbolType},
    },
};

/// Generate Rust const definitions from .config file
pub fn gen_const_command(
    config: PathBuf,
    output_dir: PathBuf,
    kconfig: PathBuf,
    srctree: PathBuf,
) -> Result<()> {
    println!("📝 Generating Rust const definitions from .config...");
    println!("Config: {}", config.display());
    println!("Output: {}", output_dir.display());

    // Parse .config file
    let config_map = parse_config(&config)?;

    // Parse Kconfig file to obtain the authoritative symbol types
    let mut parser = Parser::new(&kconfig, &srctree)?;
    let ast = parser.parse()?;
    let type_map = build_type_map(&ast.entries);

    // Generate config.rs via ConfigGenerator
    ConfigGenerator::generate_rust_consts(&output_dir, &config_map, &type_map)?;

    println!("✅ Generated config.rs successfully");

    Ok(())
}

/// Build a mapping from symbol name (without CONFIG_ prefix) to its SymbolType
/// by walking the Kconfig AST.
fn build_type_map(entries: &[Entry]) -> HashMap<String, SymbolType> {
    let mut map = HashMap::new();
    collect_types(entries, &mut map);
    map
}

fn collect_types(entries: &[Entry], map: &mut HashMap<String, SymbolType>) {
    for entry in entries {
        match entry {
            Entry::Config(config) => {
                let name = config.name.strip_prefix("CONFIG_").unwrap_or(&config.name);
                map.insert(name.to_string(), config.symbol_type.clone());
            }
            Entry::MenuConfig(mc) => {
                let name = mc.name.strip_prefix("CONFIG_").unwrap_or(&mc.name);
                map.insert(name.to_string(), mc.symbol_type.clone());
            }
            Entry::Choice(choice) => {
                for opt in &choice.options {
                    let name = opt.name.strip_prefix("CONFIG_").unwrap_or(&opt.name);
                    map.insert(name.to_string(), opt.symbol_type.clone());
                }
            }
            Entry::Menu(menu) => {
                collect_types(&menu.entries, map);
            }
            Entry::If(if_entry) => {
                collect_types(&if_entry.entries, map);
            }
            _ => {}
        }
    }
}

/// Parse .config file
/// Now expects standardized format:
/// - Bool: CONFIG_X=y or # CONFIG_X is not set
/// - Int: CONFIG_X=123 (no quotes)
/// - Hex: CONFIG_X=0xff (no quotes)
/// - String: CONFIG_X="value" (with quotes)
fn parse_config(config_path: &Path) -> Result<HashMap<String, String>> {
    let content = fs::read_to_string(config_path)?;

    let mut config = HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim();

            // Remove quotes if present (for backward compatibility)
            let value = if value.starts_with('"') && value.ends_with('"') {
                &value[1..value.len() - 1]
            } else {
                value
            };

            config.insert(key.to_string(), value.to_string());
        }
    }

    Ok(config)
}
