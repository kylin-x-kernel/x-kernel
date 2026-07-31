// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use super::{ConfigGenerator, ConfigReader, ConfigWriter};
use crate::{
    error::Result,
    kconfig::{Parser, SymbolTable, ast::Entry, expr::evaluate_expr},
    ui::dependency_resolver::{DependencyError, DependencyResolver},
};

pub struct GeneratedArtifacts {
    pub auto_conf: PathBuf,
    pub autoconf_h: PathBuf,
}

pub struct ConfigEngine {
    entries: Vec<Entry>,
    symbols: SymbolTable,
    dependency_resolver: DependencyResolver,
}

impl ConfigEngine {
    pub fn from_kconfig(kconfig: impl AsRef<Path>, srctree: impl AsRef<Path>) -> Result<Self> {
        let mut parser = Parser::new(kconfig.as_ref(), srctree.as_ref())?;
        let ast = parser.parse()?;

        let mut dependency_resolver = DependencyResolver::new();
        dependency_resolver.build_from_entries(&ast.entries);

        let mut symbols = SymbolTable::new();
        extract_symbols_with_defaults(&ast.entries, &mut symbols);

        Ok(Self {
            entries: ast.entries,
            symbols,
            dependency_resolver,
        })
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn symbols(&self) -> &SymbolTable {
        &self.symbols
    }

    pub fn symbols_mut(&mut self) -> &mut SymbolTable {
        &mut self.symbols
    }

    pub fn into_symbols(self) -> SymbolTable {
        self.symbols
    }

    pub fn into_entries_and_symbols(self) -> (Vec<Entry>, SymbolTable) {
        (self.entries, self.symbols)
    }

    pub fn into_menuconfig_parts(self) -> (Vec<Entry>, SymbolTable, DependencyResolver) {
        (self.entries, self.symbols, self.dependency_resolver)
    }

    pub fn from_parts(
        entries: Vec<Entry>,
        symbols: SymbolTable,
        dependency_resolver: DependencyResolver,
    ) -> Self {
        Self {
            entries,
            symbols,
            dependency_resolver,
        }
    }

    pub fn load_config(&mut self, path: impl AsRef<Path>) -> Result<()> {
        let values = ConfigReader::read(path)?;
        self.apply_menuconfig_values(&values);
        Ok(())
    }

    pub fn load_existing_config(
        &mut self,
        path: impl AsRef<Path>,
    ) -> Result<super::oldconfig::ConfigChanges> {
        let old_config = ConfigReader::read(path)?;
        let current_symbols: HashSet<String> = self
            .symbols
            .all_symbols()
            .map(|(name, _)| name.clone())
            .collect();
        let old_symbol_names: HashSet<String> = old_config.keys().cloned().collect();

        let mut changes = super::oldconfig::ConfigChanges::new();

        for name in &current_symbols {
            if !old_symbol_names.contains(name) {
                changes.new_symbols.push(name.clone());
                self.symbols.mark_as_new(name);
            }
        }

        for name in &old_symbol_names {
            if !current_symbols.contains(name) {
                changes.removed_symbols.push(name.clone());
            }
        }

        self.apply_config_values(&old_config);

        for name in old_symbol_names.intersection(&current_symbols) {
            self.symbols.mark_from_config(name);
        }

        self.prune_inactive_symbols();

        Ok(changes)
    }

    pub fn load_menuconfig_config(&mut self, path: impl AsRef<Path>) -> Result<()> {
        let values = ConfigReader::read(path)?;
        self.apply_menuconfig_values(&values);
        self.enforce_choice_mutual_exclusion();
        self.filter_by_if_conditions();
        self.reevaluate_defaults();
        self.clear_stale_menuconfig_conditional_defaults();
        self.prune_inactive_symbols();
        Ok(())
    }

    pub fn apply_config_values(&mut self, values: &HashMap<String, String>) {
        for (name, value) in values {
            let Some(symbol) = self.symbols.get_symbol(name) else {
                continue;
            };

            if value == "n" {
                match symbol.symbol_type {
                    crate::kconfig::ast::SymbolType::Bool
                    | crate::kconfig::ast::SymbolType::Tristate => {
                        self.symbols.set_value(name, value.clone());
                    }
                    _ => {}
                }
            } else {
                self.symbols.set_value(name, value.clone());
            }
        }
    }

    pub fn prune_inactive_symbols(&mut self) -> Vec<String> {
        let inactive: Vec<String> = self
            .symbols
            .all_symbols()
            .filter_map(|(name, _)| {
                if self
                    .dependency_resolver
                    .can_enable(name, &self.symbols)
                    .is_err()
                    && !self
                        .dependency_resolver
                        .is_selected_by_enabled_symbol(name, &self.symbols)
                {
                    Some(name.clone())
                } else {
                    None
                }
            })
            .collect();

        for name in &inactive {
            self.symbols.clear_value(name);
        }

        inactive
    }

    pub fn can_enable(&self, symbol: &str) -> std::result::Result<(), DependencyError> {
        self.dependency_resolver.can_enable(symbol, &self.symbols)
    }

    pub fn can_disable(&self, symbol: &str) -> std::result::Result<(), DependencyError> {
        self.dependency_resolver.can_disable(symbol, &self.symbols)
    }

    pub fn apply_selects(&mut self, symbol: &str) -> Vec<String> {
        self.dependency_resolver
            .apply_selects(symbol, &mut self.symbols)
    }

    pub fn get_implied_symbols(&self, symbol: &str) -> Vec<String> {
        self.dependency_resolver
            .get_implied_symbols(symbol, &self.symbols)
    }

    pub fn check_disable_cascade(&self, symbol: &str) -> Vec<String> {
        self.dependency_resolver
            .check_disable_cascade(symbol, &self.symbols)
    }

    pub fn set_value(&mut self, symbol: &str, value: String) {
        self.symbols.set_value(symbol, value);
    }

    pub fn set_value_tracked(&mut self, symbol: &str, value: String) {
        self.symbols.set_value_tracked(symbol, value);
    }

    pub fn get_value(&self, symbol: &str) -> Option<String> {
        self.symbols.get_value(symbol)
    }

    pub fn is_enabled(&self, symbol: &str) -> bool {
        self.symbols.is_enabled(symbol)
    }

    pub fn audit_dependency_violations(&self) -> Vec<String> {
        let mut violations = Vec::new();

        for (symbol_name, _symbol) in self.symbols.all_symbols() {
            if self.symbols.is_enabled(symbol_name)
                && let Err(err) = self
                    .dependency_resolver
                    .can_enable(symbol_name, &self.symbols)
                {
                    if self
                        .dependency_resolver
                        .is_selected_by_enabled_symbol(symbol_name, &self.symbols)
                    {
                        continue;
                    }
                    violations.push(format!("{}: {}", symbol_name, err));
                }
        }

        violations
    }

    pub fn clear_new_symbol_values(&mut self) {
        let new_symbol_names: Vec<String> = self
            .symbols
            .get_new_symbols()
            .iter()
            .map(|symbol| symbol.name.clone())
            .collect();

        for name in new_symbol_names {
            self.symbols.clear_value(&name);
        }
    }

    pub fn refresh_prompt_state(&mut self) {
        self.enforce_choice_mutual_exclusion();
        self.filter_by_if_conditions();
        self.reevaluate_defaults();
        self.apply_noninteractive_reverse_dependencies();
        self.reevaluate_defaults();
        self.validate_symbol_ranges();
        self.prune_inactive_symbols();
    }

    pub fn minimal_symbols_against_defaults(&self) -> SymbolTable {
        let mut minimal = SymbolTable::new();
        let ordered_symbols = prompted_symbol_names_in_order(&self.entries);
        let mut preserved_values = HashMap::<String, String>::new();

        for name in ordered_symbols {
            let Some(symbol) = self.symbols.get_symbol(&name) else {
                continue;
            };
            let current_value = symbol.value.clone();
            let mut replay = self.replay_engine();
            replay.apply_config_values(&preserved_values);
            replay.refresh_prompt_state();

            if replay.get_value(&name) == current_value {
                continue;
            }

            minimal.add_symbol(name.clone(), symbol.symbol_type.clone());
            if let Some(value) = current_value.clone() {
                preserved_values.insert(name.clone(), value.clone());
                minimal.set_value(&name, value);
            }
        }

        minimal
    }

    fn replay_engine(&self) -> Self {
        let mut dependency_resolver = DependencyResolver::new();
        dependency_resolver.build_from_entries(&self.entries);

        let mut symbols = SymbolTable::new();
        extract_symbols_with_defaults(&self.entries, &mut symbols);

        Self {
            entries: self.entries.clone(),
            symbols,
            dependency_resolver,
        }
    }

    fn apply_menuconfig_values(&mut self, values: &HashMap<String, String>) {
        for (name, value) in values {
            let Some(symbol) = self.symbols.get_symbol(name) else {
                continue;
            };

            if value == "n" {
                match symbol.symbol_type {
                    crate::kconfig::ast::SymbolType::Bool
                    | crate::kconfig::ast::SymbolType::Tristate => {
                        self.symbols.set_value(name, value.clone());
                        self.symbols.mark_from_config(name);
                    }
                    _ => {}
                }
            } else {
                self.symbols.set_value(name, value.clone());
                self.symbols.mark_from_config(name);
            }
        }
    }

    fn enforce_choice_mutual_exclusion(&mut self) {
        let mut choice_groups = HashMap::<String, Vec<String>>::new();
        collect_choice_groups_inner(&self.entries, &mut choice_groups);

        for (name, siblings) in &choice_groups {
            let is_selected_from_config = self
                .symbols
                .get_symbol(name)
                .map(|s| s.from_config && s.value.as_deref() == Some("y"))
                .unwrap_or(false);

            if is_selected_from_config {
                for sibling in siblings {
                    if sibling != name {
                        let sibling_from_config = self
                            .symbols
                            .get_symbol(sibling)
                            .map(|s| s.from_config)
                            .unwrap_or(false);
                        if !sibling_from_config {
                            self.symbols.set_value(sibling, "n".to_string());
                        }
                    }
                }
            }
        }
    }

    fn filter_by_if_conditions(&mut self) {
        let mut symbol_conditions = HashMap::<String, Vec<crate::kconfig::ast::Expr>>::new();
        collect_symbol_if_conditions(&self.entries, &[], &mut symbol_conditions);

        for (name, conditions) in &symbol_conditions {
            let all_satisfied = conditions
                .iter()
                .all(|cond| evaluate_expr(cond, &self.symbols).unwrap_or(false));

            if !all_satisfied
                && let Some(symbol) = self.symbols.get_symbol(name) {
                    match symbol.symbol_type {
                        crate::kconfig::ast::SymbolType::Bool
                        | crate::kconfig::ast::SymbolType::Tristate => {
                            self.symbols.set_value(name, "n".to_string());
                        }
                        _ => {
                            self.symbols.clear_value(name);
                        }
                    }
                }
        }
    }

    fn reevaluate_defaults(&mut self) {
        reevaluate_defaults_inner(&self.entries, &mut self.symbols);
    }

    fn clear_stale_menuconfig_conditional_defaults(&mut self) {
        clear_stale_menuconfig_conditional_defaults_inner(&self.entries, &mut self.symbols);
    }

    fn apply_noninteractive_reverse_dependencies(&mut self) {
        let mut pending: Vec<String> = self
            .symbols
            .all_symbols()
            .filter_map(|(name, _)| self.symbols.is_enabled(name).then_some(name.clone()))
            .collect();
        let mut processed = HashSet::new();

        while let Some(symbol) = pending.pop() {
            if !processed.insert(symbol.clone()) {
                continue;
            }

            for selected in self
                .dependency_resolver
                .apply_selects(&symbol, &mut self.symbols)
            {
                pending.push(selected);
            }

            for implied in self
                .dependency_resolver
                .get_implied_symbols(&symbol, &self.symbols)
            {
                let implied_from_config = self
                    .symbols
                    .get_symbol(&implied)
                    .map(|sym| sym.from_config)
                    .unwrap_or(false);
                if !implied_from_config {
                    self.symbols.set_value(&implied, "y".to_string());
                    pending.push(implied);
                }
            }
        }
    }

    fn validate_symbol_ranges(&mut self) {
        validate_symbol_ranges_inner(&self.entries, &mut self.symbols);
    }

    pub fn write_config(&self, output: impl AsRef<Path>) -> Result<()> {
        ConfigWriter::write(output, &self.symbols)
    }

    pub fn write_artifacts(&self, output: impl AsRef<Path>) -> Result<GeneratedArtifacts> {
        let output = output.as_ref();
        self.write_config(output)?;

        let output_dir = output.parent().unwrap_or(Path::new("."));
        let auto_conf = output_dir.join("auto.conf");
        let autoconf_h = output_dir.join("autoconf.h");

        ConfigGenerator::generate_auto_conf(&auto_conf, &self.symbols)?;
        ConfigGenerator::generate_autoconf_h(&autoconf_h, &self.symbols)?;

        Ok(GeneratedArtifacts {
            auto_conf,
            autoconf_h,
        })
    }
}

fn extract_symbols_with_defaults(entries: &[Entry], symbols: &mut SymbolTable) {
    extract_symbols_internal(entries, symbols, true);
}

fn collect_choice_groups_inner(entries: &[Entry], groups: &mut HashMap<String, Vec<String>>) {
    for entry in entries {
        match entry {
            Entry::Choice(choice) => {
                let option_names: Vec<String> = choice
                    .options
                    .iter()
                    .map(|option| option.name.clone())
                    .collect();
                for name in &option_names {
                    groups.insert(name.clone(), option_names.clone());
                }
            }
            Entry::Menu(menu) => collect_choice_groups_inner(&menu.entries, groups),
            Entry::If(if_entry) => collect_choice_groups_inner(&if_entry.entries, groups),
            _ => {}
        }
    }
}

fn collect_symbol_if_conditions(
    entries: &[Entry],
    parent_conditions: &[crate::kconfig::ast::Expr],
    result: &mut HashMap<String, Vec<crate::kconfig::ast::Expr>>,
) {
    for entry in entries {
        match entry {
            Entry::Config(config)
                if !parent_conditions.is_empty() => {
                    result.insert(config.name.clone(), parent_conditions.to_vec());
                }
            Entry::MenuConfig(menuconfig)
                if !parent_conditions.is_empty() => {
                    result.insert(menuconfig.name.clone(), parent_conditions.to_vec());
                }
            Entry::Choice(choice) => {
                for option in &choice.options {
                    if !parent_conditions.is_empty() {
                        result.insert(option.name.clone(), parent_conditions.to_vec());
                    }
                }
            }
            Entry::Menu(menu) => {
                collect_symbol_if_conditions(&menu.entries, parent_conditions, result);
            }
            Entry::If(if_entry) => {
                let mut new_conditions = parent_conditions.to_vec();
                new_conditions.push(if_entry.condition.clone());
                collect_symbol_if_conditions(&if_entry.entries, &new_conditions, result);
            }
            _ => {}
        }
    }
}

fn prompted_symbol_names_in_order(entries: &[Entry]) -> Vec<String> {
    let mut names = Vec::new();
    collect_prompted_symbol_names(entries, &mut names);
    names
}

fn collect_prompted_symbol_names(entries: &[Entry], result: &mut Vec<String>) {
    for entry in entries {
        match entry {
            Entry::Config(config)
                if config.properties.prompt.is_some() => {
                    result.push(
                        config
                            .name
                            .strip_prefix("CONFIG_")
                            .unwrap_or(&config.name)
                            .to_string(),
                    );
                }
            Entry::MenuConfig(menuconfig)
                if menuconfig.properties.prompt.is_some() => {
                    result.push(
                        menuconfig
                            .name
                            .strip_prefix("CONFIG_")
                            .unwrap_or(&menuconfig.name)
                            .to_string(),
                    );
                }
            Entry::Choice(choice) => {
                for option in &choice.options {
                    if option.properties.prompt.is_some() {
                        result.push(
                            option
                                .name
                                .strip_prefix("CONFIG_")
                                .unwrap_or(&option.name)
                                .to_string(),
                        );
                    }
                }
            }
            Entry::Menu(menu) => collect_prompted_symbol_names(&menu.entries, result),
            Entry::If(if_entry) => collect_prompted_symbol_names(&if_entry.entries, result),
            _ => {}
        }
    }
}

fn reevaluate_defaults_inner(entries: &[Entry], symbol_table: &mut SymbolTable) {
    for entry in entries {
        match entry {
            Entry::Config(config) => {
                let has_conditional = config
                    .properties
                    .defaults
                    .iter()
                    .any(|d| d.condition.is_some());
                if has_conditional {
                    let from_config = symbol_table
                        .get_symbol(&config.name)
                        .map(|s| s.from_config)
                        .unwrap_or(false);
                    if (config.is_derived() || !from_config)
                        && let Some(default_value) =
                            config.properties.evaluate_default(symbol_table)
                        {
                            symbol_table.set_value(&config.name, default_value);
                        }
                }
            }
            Entry::MenuConfig(menuconfig) => {
                let has_conditional = menuconfig
                    .properties
                    .defaults
                    .iter()
                    .any(|d| d.condition.is_some());
                if has_conditional {
                    let from_config = symbol_table
                        .get_symbol(&menuconfig.name)
                        .map(|s| s.from_config)
                        .unwrap_or(false);
                    if (menuconfig.is_derived() || !from_config)
                        && let Some(default_value) =
                            menuconfig.properties.evaluate_default(symbol_table)
                        {
                            symbol_table.set_value(&menuconfig.name, default_value);
                        }
                }
            }
            Entry::Menu(menu) => reevaluate_defaults_inner(&menu.entries, symbol_table),
            Entry::If(if_entry) => reevaluate_defaults_inner(&if_entry.entries, symbol_table),
            _ => {}
        }
    }
}

fn clear_stale_menuconfig_conditional_defaults_inner(
    entries: &[Entry],
    symbol_table: &mut SymbolTable,
) {
    for entry in entries {
        match entry {
            Entry::Config(config) => clear_stale_conditional_default(
                &config.name,
                &config.symbol_type,
                &config.properties,
                config.is_derived(),
                symbol_table,
            ),
            Entry::MenuConfig(menuconfig) => clear_stale_conditional_default(
                &menuconfig.name,
                &menuconfig.symbol_type,
                &menuconfig.properties,
                menuconfig.is_derived(),
                symbol_table,
            ),
            Entry::Menu(menu) => {
                clear_stale_menuconfig_conditional_defaults_inner(&menu.entries, symbol_table)
            }
            Entry::If(if_entry) => {
                clear_stale_menuconfig_conditional_defaults_inner(&if_entry.entries, symbol_table)
            }
            _ => {}
        }
    }
}

fn clear_stale_conditional_default(
    name: &str,
    symbol_type: &crate::kconfig::ast::SymbolType,
    properties: &crate::kconfig::ast::Property,
    is_derived: bool,
    symbol_table: &mut SymbolTable,
) {
    let has_conditional = properties
        .defaults
        .iter()
        .any(|default| default.condition.is_some());
    if !has_conditional {
        return;
    }

    let from_config = symbol_table
        .get_symbol(name)
        .map(|symbol| symbol.from_config)
        .unwrap_or(false);
    if from_config && !is_derived {
        return;
    }

    if properties.evaluate_default(symbol_table).is_some() {
        return;
    }

    match symbol_type {
        crate::kconfig::ast::SymbolType::Bool | crate::kconfig::ast::SymbolType::Tristate => {
            symbol_table.set_value(name, "n".to_string());
        }
        _ => {
            symbol_table.clear_value(name);
        }
    }
}

fn validate_symbol_ranges_inner(entries: &[Entry], symbol_table: &mut SymbolTable) {
    for entry in entries {
        match entry {
            Entry::Config(config) => validate_symbol_range(
                &config.name,
                &config.symbol_type,
                &config.properties,
                symbol_table,
            ),
            Entry::MenuConfig(menuconfig) => validate_symbol_range(
                &menuconfig.name,
                &menuconfig.symbol_type,
                &menuconfig.properties,
                symbol_table,
            ),
            Entry::Menu(menu) => validate_symbol_ranges_inner(&menu.entries, symbol_table),
            Entry::If(if_entry) => validate_symbol_ranges_inner(&if_entry.entries, symbol_table),
            _ => {}
        }
    }
}

fn validate_symbol_range(
    name: &str,
    symbol_type: &crate::kconfig::ast::SymbolType,
    properties: &crate::kconfig::ast::Property,
    symbol_table: &mut SymbolTable,
) {
    let Some((min_expr, max_expr, condition)) = &properties.range else {
        return;
    };

    if let Some(condition) = condition
        && !evaluate_expr(condition, symbol_table).unwrap_or(false) {
            return;
        }

    let Some(current_value) = symbol_table.get_value(name) else {
        return;
    };

    let Some(current_numeric) = parse_symbol_numeric_value(symbol_type, &current_value) else {
        return;
    };
    let Some(min_numeric) = parse_expr_numeric_value(symbol_type, min_expr, symbol_table) else {
        return;
    };
    let Some(max_numeric) = parse_expr_numeric_value(symbol_type, max_expr, symbol_table) else {
        return;
    };

    let clamped = current_numeric.clamp(min_numeric, max_numeric);
    if clamped != current_numeric {
        symbol_table.set_value(name, format_numeric_value(symbol_type, clamped));
    }
}

fn parse_expr_numeric_value(
    symbol_type: &crate::kconfig::ast::SymbolType,
    expr: &crate::kconfig::ast::Expr,
    symbol_table: &SymbolTable,
) -> Option<i128> {
    match expr {
        crate::kconfig::ast::Expr::Const(value) => parse_symbol_numeric_value(symbol_type, value),
        crate::kconfig::ast::Expr::Symbol(symbol) => symbol_table
            .get_value(symbol)
            .and_then(|value| parse_symbol_numeric_value(symbol_type, &value)),
        _ => None,
    }
}

fn parse_symbol_numeric_value(
    symbol_type: &crate::kconfig::ast::SymbolType,
    value: &str,
) -> Option<i128> {
    match symbol_type {
        crate::kconfig::ast::SymbolType::Hex => {
            let normalized = value
                .strip_prefix("0x")
                .or_else(|| value.strip_prefix("0X"))
                .unwrap_or(value);
            i128::from_str_radix(normalized, 16).ok()
        }
        ty if ty.is_integer_type() => value.parse::<i128>().ok(),
        _ => None,
    }
}

fn format_numeric_value(symbol_type: &crate::kconfig::ast::SymbolType, value: i128) -> String {
    match symbol_type {
        crate::kconfig::ast::SymbolType::Hex => format!("0x{:x}", value),
        _ => value.to_string(),
    }
}

fn extract_symbols_internal(entries: &[Entry], symbols: &mut SymbolTable, apply_defaults: bool) {
    for entry in entries {
        match entry {
            Entry::Config(config) => {
                let clean_name = config.name.strip_prefix("CONFIG_").unwrap_or(&config.name);
                symbols.add_symbol(clean_name.to_string(), config.symbol_type.clone());

                if apply_defaults {
                    if let Some(default_value) = config.properties.evaluate_default(symbols) {
                        symbols.set_value(clean_name, default_value);
                    } else if matches!(
                        config.symbol_type,
                        crate::kconfig::ast::SymbolType::Bool
                            | crate::kconfig::ast::SymbolType::Tristate
                    ) {
                        symbols.set_value(clean_name, "n".to_string());
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
                    if let Some(default_value) = menuconfig.properties.evaluate_default(symbols) {
                        symbols.set_value(clean_name, default_value);
                    } else if matches!(
                        menuconfig.symbol_type,
                        crate::kconfig::ast::SymbolType::Bool
                            | crate::kconfig::ast::SymbolType::Tristate
                    ) {
                        symbols.set_value(clean_name, "n".to_string());
                    }
                }
            }
            Entry::Choice(choice) => {
                for option in &choice.options {
                    let clean_name = option.name.strip_prefix("CONFIG_").unwrap_or(&option.name);
                    symbols.add_symbol(clean_name.to_string(), option.symbol_type.clone());
                }

                if apply_defaults {
                    if let Some(default_name) = &choice.default {
                        let clean_default =
                            default_name.strip_prefix("CONFIG_").unwrap_or(default_name);
                        symbols.set_value(clean_default, "y".to_string());
                    } else if let Some(first_option) = choice.options.first() {
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
                let condition_met =
                    apply_defaults && evaluate_expr(&if_entry.condition, symbols).unwrap_or(false);
                extract_symbols_internal(&if_entry.entries, symbols, condition_met);
            }
            _ => {}
        }
    }
}
