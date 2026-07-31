// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{fmt::Write, path::Path};

use crate::{config::write_if_changed, error::Result, kconfig::SymbolTable};

pub struct ConfigWriter;

impl ConfigWriter {
    pub fn write(path: impl AsRef<Path>, symbols: &SymbolTable) -> Result<()> {
        let mut content = String::new();

        writeln!(content, "#").unwrap();
        writeln!(content, "# Automatically generated file; DO NOT EDIT.").unwrap();
        writeln!(content, "# Rust Kbuild Configuration").unwrap();
        writeln!(content, "#").unwrap();

        // Sort keys alphabetically for stable output
        let mut sorted_symbols: Vec<_> = symbols.all_symbols().collect();
        sorted_symbols.sort_by_key(|(name, _)| *name);

        for (name, symbol) in sorted_symbols {
            let clean_name = name.strip_prefix("CONFIG_").unwrap_or(name);

            if let Some(value) = &symbol.value {
                match value.as_str() {
                    "y" | "m" => {
                        writeln!(content, "{}={}", clean_name, value).unwrap();
                    }
                    "n" => {
                        writeln!(content, "# {} is not set", clean_name).unwrap();
                    }
                    _ => {
                        use crate::kconfig::ast::SymbolType;
                        match symbol.symbol_type {
                            SymbolType::Hex => {
                                // Hex: NO quotes, normalize to 0x format
                                let normalized_hex =
                                    if value.starts_with("0x") || value.starts_with("0X") {
                                        format!("0x{}", value[2..].to_lowercase())
                                    } else if let Ok(num) = value.parse::<u64>() {
                                        format!("0x{:x}", num)
                                    } else {
                                        // If parsing fails, use the value as-is
                                        value.to_string()
                                    };
                                writeln!(content, "{}={}", clean_name, normalized_hex).unwrap();
                            }
                            ref ty if ty.is_integer_type() => {
                                // Integer: NO quotes, decimal format
                                writeln!(content, "{}={}", clean_name, value).unwrap();
                            }
                            SymbolType::String => {
                                // String: Keep quotes
                                writeln!(content, "{}=\"{}\"", clean_name, value).unwrap();
                            }
                            SymbolType::Range(_) => {
                                // Range: [a,b,c] format without extra quotes
                                writeln!(content, "{}={}", clean_name, value).unwrap();
                            }
                            _ => {
                                // Fallback for other types
                                writeln!(content, "{}=\"{}\"", clean_name, value).unwrap();
                            }
                        }
                    }
                }
            } else {
                // Only use "is not set" for bool/tristate types
                use crate::kconfig::ast::SymbolType;
                match symbol.symbol_type {
                    SymbolType::Bool | SymbolType::Tristate => {
                        writeln!(content, "# {} is not set", clean_name).unwrap();
                    }
                    _ => {
                        // For other types (string, int, hex, range), skip writing to keep the config clean
                    }
                }
            }
        }

        write_if_changed(path.as_ref(), &content)?;
        Ok(())
    }
}
