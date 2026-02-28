use crate::kconfig::ast::{Entry, Expr, Property};
use crate::kconfig::symbol::SymbolTable;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct Dependency {
    pub symbol: String,
    pub condition: Option<Expr>,
}

#[derive(Debug, Clone)]
pub struct Selection {
    pub symbol: String,
    pub condition: Option<Expr>,
}

#[derive(Debug, Clone)]
pub struct Implication {
    pub symbol: String,
    pub condition: Option<Expr>,
}

pub struct DependencyResolver {
    /// Map: symbol -> list of symbols it depends on
    depends_map: HashMap<String, Vec<Dependency>>,

    /// Map: symbol -> list of symbols it selects
    select_map: HashMap<String, Vec<Selection>>,

    /// Map: symbol -> list of symbols it implies
    imply_map: HashMap<String, Vec<Implication>>,

    /// Map: symbol -> list of symbols that select it (reverse dependencies)
    reverse_select_map: HashMap<String, Vec<String>>,

    /// Expression evaluator
    expr_evaluator: ExprEvaluator,
}

impl DependencyResolver {
    pub fn new() -> Self {
        Self {
            depends_map: HashMap::new(),
            select_map: HashMap::new(),
            imply_map: HashMap::new(),
            reverse_select_map: HashMap::new(),
            expr_evaluator: ExprEvaluator::new(),
        }
    }

    /// Build dependency maps from Kconfig AST
    pub fn build_from_entries(&mut self, entries: &[Entry]) {
        self.process_entries(entries);
    }

    fn process_entries(&mut self, entries: &[Entry]) {
        for entry in entries {
            match entry {
                Entry::Config(cfg) => {
                    self.process_config(&cfg.name, &cfg.properties);
                }
                Entry::MenuConfig(mcfg) => {
                    self.process_config(&mcfg.name, &mcfg.properties);
                }
                Entry::Menu(menu) => {
                    self.process_entries(&menu.entries);
                }
                Entry::If(if_block) => {
                    self.process_entries(&if_block.entries);
                }
                Entry::Choice(choice) => {
                    for option in &choice.options {
                        self.process_config(&option.name, &option.properties);
                    }
                }
                _ => {}
            }
        }
    }

    fn process_config(&mut self, name: &str, properties: &Property) {
        // Extract depends
        if let Some(depends_expr) = &properties.depends {
            let deps = self.extract_symbols_from_expr(depends_expr);
            self.depends_map.insert(
                name.to_string(),
                deps.into_iter()
                    .map(|s| Dependency {
                        symbol: s,
                        condition: Some(depends_expr.clone()),
                    })
                    .collect(),
            );
        }

        // Extract selects
        if !properties.select.is_empty() {
            let mut selections = Vec::new();
            for (selected_symbol, condition) in &properties.select {
                selections.push(Selection {
                    symbol: selected_symbol.clone(),
                    condition: condition.clone(),
                });

                // Build reverse map
                self.reverse_select_map
                    .entry(selected_symbol.clone())
                    .or_insert_with(Vec::new)
                    .push(name.to_string());
            }
            self.select_map.insert(name.to_string(), selections);
        }

        // Extract implies
        if !properties.imply.is_empty() {
            let implications: Vec<Implication> = properties
                .imply
                .iter()
                .map(|(symbol, condition)| Implication {
                    symbol: symbol.clone(),
                    condition: condition.clone(),
                })
                .collect();
            self.imply_map.insert(name.to_string(), implications);
        }
    }

    fn extract_symbols_from_expr(&self, expr: &Expr) -> Vec<String> {
        let mut symbols = Vec::new();
        self.collect_symbols(expr, &mut symbols);
        symbols
    }

    fn collect_symbols(&self, expr: &Expr, symbols: &mut Vec<String>) {
        match expr {
            Expr::Symbol(name) => symbols.push(name.clone()),
            Expr::And(left, right)
            | Expr::Or(left, right)
            | Expr::Equal(left, right)
            | Expr::NotEqual(left, right)
            | Expr::Less(left, right)
            | Expr::LessEqual(left, right)
            | Expr::Greater(left, right)
            | Expr::GreaterEqual(left, right) => {
                self.collect_symbols(left, symbols);
                self.collect_symbols(right, symbols);
            }
            Expr::Not(inner) => self.collect_symbols(inner, symbols),
            _ => {}
        }
    }

    /// Check if a symbol can be enabled (all dependencies met)
    pub fn can_enable(
        &self,
        symbol: &str,
        symbol_table: &SymbolTable,
    ) -> Result<(), DependencyError> {
        if let Some(deps) = self.depends_map.get(symbol) {
            // Note: All deps have the same condition (the full depends expression),
            // so we only need to check it once
            if let Some(first_dep) = deps.first() {
                if let Some(condition) = &first_dep.condition {
                    if !self.expr_evaluator.evaluate(condition, symbol_table) {
                        return Err(DependencyError::ConditionNotMet {
                            symbol: symbol.to_string(),
                            condition: self.format_expr(condition),
                        });
                    }
                }
            }
        }

        Ok(())
    }

    /// Check if a symbol can be disabled (nothing selects it)
    pub fn can_disable(
        &self,
        symbol: &str,
        symbol_table: &SymbolTable,
    ) -> Result<(), DependencyError> {
        if let Some(selectors) = self.reverse_select_map.get(symbol) {
            for selector in selectors {
                if symbol_table.is_enabled(selector) {
                    return Err(DependencyError::SelectedBy {
                        symbol: symbol.to_string(),
                        selector: selector.clone(),
                    });
                }
            }
        }

        Ok(())
    }

    /// Apply select cascade when enabling a symbol
    pub fn apply_selects(&self, symbol: &str, symbol_table: &mut SymbolTable) -> Vec<String> {
        let mut enabled = Vec::new();

        if let Some(selections) = self.select_map.get(symbol) {
            for selection in selections {
                // Check condition
                let should_select = if let Some(condition) = &selection.condition {
                    self.expr_evaluator.evaluate(condition, symbol_table)
                } else {
                    true
                };

                if should_select && !symbol_table.is_enabled(&selection.symbol) {
                    symbol_table.set_value(&selection.symbol, "y".to_string());
                    enabled.push(selection.symbol.clone());

                    // Recursively apply selects
                    let cascaded = self.apply_selects(&selection.symbol, symbol_table);
                    enabled.extend(cascaded);
                }
            }
        }

        enabled
    }

    /// Apply imply suggestions when enabling a symbol
    pub fn get_implied_symbols(&self, symbol: &str, symbol_table: &SymbolTable) -> Vec<String> {
        let mut implied = Vec::new();

        if let Some(implications) = self.imply_map.get(symbol) {
            for implication in implications {
                let should_imply = if let Some(condition) = &implication.condition {
                    self.expr_evaluator.evaluate(condition, symbol_table)
                } else {
                    true
                };

                if should_imply && !symbol_table.is_enabled(&implication.symbol) {
                    // FIX: Check if implied symbol's dependencies are satisfied
                    if self.can_enable(&implication.symbol, symbol_table).is_ok() {
                        implied.push(implication.symbol.clone());
                    }
                    // If can_enable() fails, silently skip (don't suggest)
                }
            }
        }

        implied
    }

    /// Check for conflicts when disabling a symbol
    pub fn check_disable_cascade(&self, symbol: &str, symbol_table: &SymbolTable) -> Vec<String> {
        let mut affected = Vec::new();

        // Find all symbols that depend on this one
        for (dependent, deps) in &self.depends_map {
            if symbol_table.is_enabled(dependent) {
                for dep in deps {
                    if dep.symbol == symbol {
                        affected.push(dependent.clone());
                    }
                }
            }
        }

        affected
    }

    /// Format an expression as a human-readable string
    fn format_expr(&self, expr: &Expr) -> String {
        match expr {
            Expr::Symbol(s) => s.clone(),
            Expr::Const(c) => c.clone(),
            Expr::ShellExpr(e) => format!("shell({})", e),
            Expr::Not(inner) => format!("!{}", self.format_expr(inner)),
            Expr::And(left, right) => format!(
                "({} && {})",
                self.format_expr(left),
                self.format_expr(right)
            ),
            Expr::Or(left, right) => format!(
                "({} || {})",
                self.format_expr(left),
                self.format_expr(right)
            ),
            Expr::Equal(left, right) => {
                format!("{} = {}", self.format_expr(left), self.format_expr(right))
            }
            Expr::NotEqual(left, right) => {
                format!("{} != {}", self.format_expr(left), self.format_expr(right))
            }
            Expr::Less(left, right) => {
                format!("{} < {}", self.format_expr(left), self.format_expr(right))
            }
            Expr::LessEqual(left, right) => {
                format!("{} <= {}", self.format_expr(left), self.format_expr(right))
            }
            Expr::Greater(left, right) => {
                format!("{} > {}", self.format_expr(left), self.format_expr(right))
            }
            Expr::GreaterEqual(left, right) => {
                format!("{} >= {}", self.format_expr(left), self.format_expr(right))
            }
        }
    }
}

impl Default for DependencyResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub enum DependencyError {
    DependencyNotMet { symbol: String, required: String },
    ConditionNotMet { symbol: String, condition: String },
    SelectedBy { symbol: String, selector: String },
    CircularDependency { chain: Vec<String> },
}

impl std::fmt::Display for DependencyError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            DependencyError::DependencyNotMet { symbol, required } => {
                write!(
                    f,
                    "Cannot enable {}: requires {} to be enabled first",
                    symbol, required
                )
            }
            DependencyError::ConditionNotMet { symbol, condition } => {
                write!(
                    f,
                    "Cannot enable {}: dependency not met: {}",
                    symbol, condition
                )
            }
            DependencyError::SelectedBy { symbol, selector } => {
                write!(f, "Cannot disable {}: selected by {}", symbol, selector)
            }
            DependencyError::CircularDependency { chain } => {
                write!(f, "Circular dependency: {}", chain.join(" -> "))
            }
        }
    }
}

impl std::error::Error for DependencyError {}

/// Simple expression evaluator
pub struct ExprEvaluator;

impl ExprEvaluator {
    pub fn new() -> Self {
        Self
    }

    pub fn evaluate(&self, expr: &Expr, symbol_table: &SymbolTable) -> bool {
        match expr {
            Expr::Symbol(name) => symbol_table.is_enabled(name),
            Expr::Const(val) => {
                let val_lower = val.to_lowercase();
                val_lower == "y" || val_lower == "m"
            }
            Expr::And(left, right) => {
                self.evaluate(left, symbol_table) && self.evaluate(right, symbol_table)
            }
            Expr::Or(left, right) => {
                self.evaluate(left, symbol_table) || self.evaluate(right, symbol_table)
            }
            Expr::Not(inner) => !self.evaluate(inner, symbol_table),
            Expr::Equal(left, right) => {
                self.get_expr_value(left, symbol_table) == self.get_expr_value(right, symbol_table)
            }
            Expr::NotEqual(left, right) => {
                self.get_expr_value(left, symbol_table) != self.get_expr_value(right, symbol_table)
            }
            _ => false,
        }
    }

    fn get_expr_value(&self, expr: &Expr, symbol_table: &SymbolTable) -> String {
        match expr {
            Expr::Symbol(name) => symbol_table
                .get_value(name)
                .unwrap_or_else(|| "n".to_string()),
            Expr::Const(val) => val.clone(),
            _ => "n".to_string(),
        }
    }
}
