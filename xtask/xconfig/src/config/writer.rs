use std::{fs::File, io::Write, path::Path};

use crate::{error::Result, kconfig::SymbolTable};

pub struct ConfigWriter;

impl ConfigWriter {
    pub fn write(path: impl AsRef<Path>, symbols: &SymbolTable) -> Result<()> {
        let mut file = File::create(path)?;

        writeln!(file, "#")?;
        writeln!(file, "# Automatically generated file; DO NOT EDIT.")?;
        writeln!(file, "# Rust Kbuild Configuration")?;
        writeln!(file, "#")?;

        // Sort keys alphabetically for stable output
        let mut sorted_symbols: Vec<_> = symbols.all_symbols().collect();
        sorted_symbols.sort_by(|(name_a, _), (name_b, _)| name_a.cmp(name_b));

        for (name, symbol) in sorted_symbols {
            let clean_name = name.strip_prefix("CONFIG_").unwrap_or(name);

            if let Some(value) = &symbol.value {
                match value.as_str() {
                    "y" | "m" => {
                        writeln!(file, "{}={}", clean_name, value)?;
                    }
                    "n" => {
                        writeln!(file, "# {} is not set", clean_name)?;
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
                                writeln!(file, "{}={}", clean_name, normalized_hex)?;
                            }
                            ref ty if ty.is_integer_type() => {
                                // Integer: NO quotes, decimal format
                                writeln!(file, "{}={}", clean_name, value)?;
                            }
                            SymbolType::String => {
                                // String: Keep quotes
                                writeln!(file, "{}=\"{}\"", clean_name, value)?;
                            }
                            SymbolType::Range(_) => {
                                // Range: [a,b,c] format without extra quotes
                                writeln!(file, "{}={}", clean_name, value)?;
                            }
                            _ => {
                                // Fallback for other types
                                writeln!(file, "{}=\"{}\"", clean_name, value)?;
                            }
                        }
                    }
                }
            } else {
                // Only use "is not set" for bool/tristate types
                use crate::kconfig::ast::SymbolType;
                match symbol.symbol_type {
                    SymbolType::Bool | SymbolType::Tristate => {
                        writeln!(file, "# {} is not set", clean_name)?;
                    }
                    _ => {
                        // For other types (string, int, hex, range), skip writing to keep the config clean
                    }
                }
            }
        }

        Ok(())
    }
}
