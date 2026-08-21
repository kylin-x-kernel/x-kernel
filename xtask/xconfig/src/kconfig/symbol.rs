// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::collections::HashMap;

use crate::kconfig::ast::SymbolType;

#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub symbol_type: SymbolType,
    pub value: Option<String>,
    pub is_choice: bool,
    pub is_new: bool,      // Mark as new symbol
    pub from_config: bool, // Loaded from .config
}

pub struct SymbolTable {
    symbols: HashMap<String, Symbol>,
    changed_symbols: Vec<String>, // Track modified symbols
}

impl SymbolTable {
    pub fn new() -> Self {
        Self {
            symbols: HashMap::new(),
            changed_symbols: Vec::new(),
        }
    }

    pub fn add_symbol(&mut self, name: String, symbol_type: SymbolType) {
        self.symbols.entry(name.clone()).or_insert(Symbol {
            name,
            symbol_type,
            value: None,
            is_choice: false,
            is_new: false,
            from_config: false,
        });
    }

    /// Set a symbol's value, canonicalizing it for its type.
    ///
    /// Values pass through [`canonical_symbol_value`] so every reader of the
    /// table observes one canonical spelling (notably for hex values).
    pub fn set_value(&mut self, name: &str, value: String) {
        if let Some(symbol) = self.symbols.get_mut(name) {
            symbol.value = Some(canonical_symbol_value(&symbol.symbol_type, value));
        }
    }

    pub fn get_value(&self, name: &str) -> Option<String> {
        self.symbols.get(name).and_then(|s| s.value.clone())
    }

    pub fn is_enabled(&self, name: &str) -> bool {
        self.symbols
            .get(name)
            .and_then(|s| s.value.as_ref())
            .map(|v| v == "y" || v == "m")
            .unwrap_or(false)
    }

    pub fn get_symbol(&self, name: &str) -> Option<&Symbol> {
        self.symbols.get(name)
    }

    pub fn get_symbol_mut(&mut self, name: &str) -> Option<&mut Symbol> {
        self.symbols.get_mut(name)
    }

    pub fn all_symbols(&self) -> impl Iterator<Item = (&String, &Symbol)> {
        self.symbols.iter()
    }

    /// Mark a symbol as newly added
    pub fn mark_as_new(&mut self, name: &str) {
        if let Some(symbol) = self.symbols.get_mut(name) {
            symbol.is_new = true;
        }
    }

    /// Mark a symbol as loaded from config file
    pub fn mark_from_config(&mut self, name: &str) {
        if let Some(symbol) = self.symbols.get_mut(name) {
            symbol.from_config = true;
        }
    }

    /// Clear the value of a symbol (set to None)
    pub fn clear_value(&mut self, name: &str) {
        if let Some(symbol) = self.symbols.get_mut(name) {
            symbol.value = None;
        }
    }

    /// Get all new symbols
    pub fn get_new_symbols(&self) -> Vec<&Symbol> {
        self.symbols.values().filter(|s| s.is_new).collect()
    }

    /// Set value and track the change
    ///
    /// The value is canonicalized before comparison, so format-only edits
    /// (e.g. hex case changes) are not tracked as semantic changes.
    pub fn set_value_tracked(&mut self, name: &str, value: String) {
        if let Some(symbol) = self.symbols.get_mut(name) {
            // Normalize before comparing so case-only or format-only edits of
            // a hex value are not reported as semantic changes.
            let canonical_value = canonical_symbol_value(&symbol.symbol_type, value);
            let old_value = symbol.value.clone();
            symbol.value = Some(canonical_value.clone());

            // Track if value actually changed
            if old_value != Some(canonical_value)
                && !self.changed_symbols.contains(&name.to_string()) {
                    self.changed_symbols.push(name.to_string());
                }
        }
    }

    /// Get all changed symbols
    pub fn get_changed_symbols(&self) -> &[String] {
        &self.changed_symbols
    }
}

impl Default for SymbolTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Return the canonical textual form of a symbol value for its type.
///
/// Hex values are normalized to a lowercase `0x`-prefixed form with no
/// leading zeros (an unprefixed digit string is interpreted as hex, matching
/// Kconfig semantics). Every consumer of the symbol table — `.config`,
/// `auto.conf`, `autoconf.h`, and the generated `config.rs` — must emit
/// byte-identical output for semantically equal inputs. Without this
/// normalization, a defconfig carrying `0x1C200` is rewritten as `0x1c200`
/// in `.config` while generators that emit the raw value keep the uppercase
/// form; the next build then regenerates `config.rs` with the lowercase form,
/// invalidating Cargo build-script fingerprints and cascading into a
/// workspace-wide rebuild.
///
/// Non-hex types and unparseable hex values are returned unchanged; emitters
/// keep their own diagnostics for malformed values.
pub fn canonical_symbol_value(symbol_type: &SymbolType, value: String) -> String {
    if !matches!(symbol_type, SymbolType::Hex) {
        return value;
    }

    let digits = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value.as_str());

    match u128::from_str_radix(digits, 16) {
        Ok(number) => format!("0x{number:x}"),
        Err(_) => value,
    }
}
