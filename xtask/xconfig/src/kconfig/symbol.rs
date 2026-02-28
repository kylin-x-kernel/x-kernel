use crate::kconfig::ast::SymbolType;
use std::collections::HashMap;

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

    pub fn set_value(&mut self, name: &str, value: String) {
        if let Some(symbol) = self.symbols.get_mut(name) {
            symbol.value = Some(value);
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
    pub fn set_value_tracked(&mut self, name: &str, value: String) {
        if let Some(symbol) = self.symbols.get_mut(name) {
            let old_value = symbol.value.clone();
            symbol.value = Some(value.clone());

            // Track if value actually changed
            if old_value != Some(value) {
                if !self.changed_symbols.contains(&name.to_string()) {
                    self.changed_symbols.push(name.to_string());
                }
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
