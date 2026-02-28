use crate::kconfig::ast::{Choice, Comment, Config, Entry, Menu, MenuConfig};
use crate::kconfig::{Expr, SymbolType};
use std::collections::HashMap;
use std::io::Write;

use crate::debug_log;

// Helper function to normalize menu/comment IDs by replacing spaces with underscores
// This ensures consistent ID format and prevents issues with space-containing keys
fn normalize_id(prefix: &str, text: &str) -> String {
    format!("{}_{}", prefix, text.replace(' ', "_"))
}

#[derive(Debug, Clone)]
pub struct MenuItem {
    pub id: String,
    pub kind: MenuItemKind,
    pub label: String,
    pub value: Option<ConfigValue>,
    pub is_visible: bool,
    pub is_enabled: bool,
    pub has_children: bool,
    pub depth: usize,
    pub help_text: Option<String>,
    pub depends_on: Option<Expr>,
    pub selects: Vec<String>,
    pub implies: Vec<String>,
    pub selected_by: Vec<String>,
    pub implied_by: Vec<String>,
    pub parent_choice: Option<String>,
    pub has_prompt: bool,
}

#[derive(Debug, Clone)]
pub enum MenuItemKind {
    Menu { title: String },
    Config { symbol_type: SymbolType },
    MenuConfig { symbol_type: SymbolType },
    Choice { options: Vec<String> },
    Comment { text: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConfigValue {
    Bool(bool),
    Tristate(TristateValue),
    String(String),
    Int(i64),
    Hex(String),
    Range(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum TristateValue {
    Yes,
    No,
    Module,
}

impl MenuItem {
    pub fn from_config(config: &Config, depth: usize) -> Self {
        Self {
            id: config.name.clone(),
            kind: MenuItemKind::Config {
                symbol_type: config.symbol_type.clone(),
            },
            label: config
                .properties
                .prompt
                .clone()
                .unwrap_or_else(|| config.name.clone()),
            value: None,
            is_visible: true,
            is_enabled: true,
            has_children: false,
            depth,
            help_text: config.properties.help.clone(),
            depends_on: config.properties.depends.clone(),
            selects: config
                .properties
                .select
                .iter()
                .map(|(s, _)| s.clone())
                .collect(),
            implies: config
                .properties
                .imply
                .iter()
                .map(|(s, _)| s.clone())
                .collect(),
            selected_by: Vec::new(),
            implied_by: Vec::new(),
            parent_choice: None,
            has_prompt: config.properties.prompt.is_some(),
        }
    }

    pub fn from_menuconfig(config: &MenuConfig, depth: usize) -> Self {
        Self {
            id: config.name.clone(),
            kind: MenuItemKind::MenuConfig {
                symbol_type: config.symbol_type.clone(),
            },
            label: config
                .properties
                .prompt
                .clone()
                .unwrap_or_else(|| config.name.clone()),
            value: None,
            is_visible: true,
            is_enabled: true,
            has_children: true,
            depth,
            help_text: config.properties.help.clone(),
            depends_on: config.properties.depends.clone(),
            selects: config
                .properties
                .select
                .iter()
                .map(|(s, _)| s.clone())
                .collect(),
            implies: config
                .properties
                .imply
                .iter()
                .map(|(s, _)| s.clone())
                .collect(),
            selected_by: Vec::new(),
            implied_by: Vec::new(),
            parent_choice: None,
            has_prompt: config.properties.prompt.is_some(),
        }
    }

    pub fn from_menu(menu: &Menu, depth: usize) -> Self {
        Self {
            id: normalize_id("menu", &menu.title),
            kind: MenuItemKind::Menu {
                title: menu.title.clone(),
            },
            label: menu.title.clone(),
            value: None,
            is_visible: true,
            is_enabled: true,
            has_children: true,
            depth,
            help_text: None,
            depends_on: menu.depends.clone(),
            selects: Vec::new(),
            implies: Vec::new(),
            selected_by: Vec::new(),
            implied_by: Vec::new(),
            parent_choice: None,
            has_prompt: true,
        }
    }

    pub fn from_choice(choice: &Choice, depth: usize) -> Self {
        let options: Vec<String> = choice.options.iter().map(|c| c.name.clone()).collect();
        Self {
            id: choice.name.clone().unwrap_or_else(|| "choice".to_string()),
            kind: MenuItemKind::Choice {
                options: options.clone(),
            },
            label: choice
                .prompt
                .clone()
                .unwrap_or_else(|| "Choice".to_string()),
            value: None,
            is_visible: true,
            is_enabled: true,
            has_children: !options.is_empty(),
            depth,
            help_text: None,
            depends_on: choice.depends.clone(),
            selects: Vec::new(),
            implies: Vec::new(),
            selected_by: Vec::new(),
            implied_by: Vec::new(),
            parent_choice: None,
            has_prompt: choice.prompt.is_some(),
        }
    }

    pub fn from_comment(comment: &Comment, depth: usize) -> Self {
        Self {
            id: normalize_id("comment", &comment.text),
            kind: MenuItemKind::Comment {
                text: comment.text.clone(),
            },
            label: comment.text.clone(),
            value: None,
            is_visible: true,
            is_enabled: true,
            has_children: false,
            depth,
            help_text: None,
            depends_on: comment.depends.clone(),
            selects: Vec::new(),
            implies: Vec::new(),
            selected_by: Vec::new(),
            implied_by: Vec::new(),
            parent_choice: None,
            has_prompt: true,
        }
    }
}

pub struct NavigationState {
    pub current_path: Vec<String>,
    pub selected_index: usize,
    pub scroll_offset: usize,
}

impl NavigationState {
    pub fn new() -> Self {
        Self {
            current_path: Vec::new(),
            selected_index: 0,
            scroll_offset: 0,
        }
    }
}

impl Default for NavigationState {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ConfigState {
    pub all_items: Vec<MenuItem>,
    pub menu_tree: HashMap<String, Vec<MenuItem>>,
    pub modified_symbols: HashMap<String, String>,
    pub original_values: HashMap<String, String>,
}

impl ConfigState {
    pub fn new() -> Self {
        Self {
            all_items: Vec::new(),
            menu_tree: HashMap::new(),
            modified_symbols: HashMap::new(),
            original_values: HashMap::new(),
        }
    }

    pub fn build_from_entries(entries: &[Entry]) -> Self {
        let mut state = Self::new();
        state.process_entries(entries, 0, "root");
        state.build_reverse_dependencies();
        state
    }

    fn process_entries(&mut self, entries: &[Entry], depth: usize, parent_id: &str) {
        let mut items = Vec::new();

        debug_log!(
            "[MenuTree] Building: parent_id=\"{}\", depth={}, entries_count={}",
            parent_id,
            depth,
            entries.len()
        );

        // Process entries and collect them into items
        self.collect_items(entries, depth, parent_id, &mut items, None);

        debug_log!(
            "[MenuTree] Inserting into menu_tree: key=\"{}\", items_count={}",
            parent_id,
            items.len()
        );
        for (i, item) in items.iter().enumerate() {
            debug_log!(
                "    [{}] id=\"{}\", label=\"{}\", kind={:?}",
                i,
                item.id,
                item.label,
                match &item.kind {
                    MenuItemKind::Menu { .. } => "Menu",
                    MenuItemKind::Config { .. } => "Config",
                    MenuItemKind::MenuConfig { .. } => "MenuConfig",
                    MenuItemKind::Choice { .. } => "Choice",
                    MenuItemKind::Comment { .. } => "Comment",
                }
            );
        }

        if self.menu_tree.contains_key(parent_id) {
            let existing_items = &self.menu_tree[parent_id];
            debug_log!("⚠️  WARNING: Overwriting menu_tree key: \"{}\"", parent_id);
            debug_log!(
                "    Existing {} items will be replaced with {} new items",
                existing_items.len(),
                items.len()
            );
            if !existing_items.is_empty() {
                debug_log!(
                    "    First existing item: id=\"{}\", label=\"{}\"",
                    existing_items[0].id,
                    existing_items[0].label
                );
            }
        }

        self.menu_tree.insert(parent_id.to_string(), items.clone());
        self.all_items.extend(items);
    }

    /// Recursively collects menu items from entries and appends them to the provided items vector.
    ///
    /// This helper function is used to handle inline processing of `if` blocks, ensuring that
    /// entries within if blocks are collected into the same items vector as their siblings.
    /// This prevents menu tree overwrites that would occur if if-blocks were processed via
    /// separate calls to `process_entries()`.
    ///
    /// The `if_condition` parameter is used to propagate if-block conditions to items inside them,
    /// combining with their existing depends_on expressions.
    fn collect_items(
        &mut self,
        entries: &[Entry],
        depth: usize,
        parent_id: &str,
        items: &mut Vec<MenuItem>,
        if_condition: Option<&Expr>,
    ) {
        debug_log!(
            "[MenuTree] collect_items: parent_id=\"{}\", depth={}, entries_count={}, if_condition={}",
            parent_id,
            depth,
            entries.len(),
            if_condition.is_some()
        );

        for entry in entries {
            match entry {
                Entry::Config(config) => {
                    let mut item = MenuItem::from_config(config, depth);
                    // Combine if_condition with existing depends_on
                    if let Some(if_cond) = if_condition {
                        item.depends_on =
                            Some(Self::combine_conditions(item.depends_on.as_ref(), if_cond));
                    }
                    items.push(item);
                }
                Entry::MenuConfig(menuconfig) => {
                    let mut item = MenuItem::from_menuconfig(menuconfig, depth);
                    // Combine if_condition with existing depends_on
                    if let Some(if_cond) = if_condition {
                        item.depends_on =
                            Some(Self::combine_conditions(item.depends_on.as_ref(), if_cond));
                    }
                    items.push(item.clone());

                    // MenuConfig can have sub-items (not in this simple version)
                    // In a full implementation, we'd recursively process
                }
                Entry::Menu(menu) => {
                    let mut item = MenuItem::from_menu(menu, depth);
                    // Combine if_condition with existing depends_on
                    if let Some(if_cond) = if_condition {
                        item.depends_on =
                            Some(Self::combine_conditions(item.depends_on.as_ref(), if_cond));
                    }
                    let menu_id = item.id.clone();
                    items.push(item);

                    // Process menu children with new parent_id and depth
                    self.process_entries(&menu.entries, depth + 1, &menu_id);
                }
                Entry::Choice(choice) => {
                    // Generate unique choice ID if not named
                    let choice_id = if let Some(name) = &choice.name {
                        name.clone()
                    } else {
                        // Use the first option name to generate a unique ID
                        if let Some(first_option) = choice.options.first() {
                            format!("choice_{}", first_option.name)
                        } else {
                            "choice_unknown".to_string()
                        }
                    };

                    let mut item = MenuItem::from_choice(choice, depth);
                    item.id = choice_id.clone();
                    // Combine if_condition with existing depends_on
                    if let Some(if_cond) = if_condition {
                        item.depends_on =
                            Some(Self::combine_conditions(item.depends_on.as_ref(), if_cond));
                    }
                    items.push(item);

                    // Add choice options as children with parent_choice set
                    for option in &choice.options {
                        let mut opt_item = MenuItem::from_config(option, depth + 1);
                        opt_item.parent_choice = Some(choice_id.clone());
                        // Combine if_condition with option's depends_on as well
                        if let Some(if_cond) = if_condition {
                            opt_item.depends_on = Some(Self::combine_conditions(
                                opt_item.depends_on.as_ref(),
                                if_cond,
                            ));
                        }
                        items.push(opt_item);
                    }
                }
                Entry::Comment(comment) => {
                    let mut item = MenuItem::from_comment(comment, depth);
                    // Combine if_condition with existing depends_on
                    if let Some(if_cond) = if_condition {
                        item.depends_on =
                            Some(Self::combine_conditions(item.depends_on.as_ref(), if_cond));
                    }
                    items.push(item);
                }
                Entry::If(if_entry) => {
                    // Process if block entries inline - they belong to the same menu level
                    // Propagate the if condition by combining it with any existing if_condition
                    let combined_condition = if let Some(outer_if_cond) = if_condition {
                        Self::combine_conditions(Some(outer_if_cond), &if_entry.condition)
                    } else {
                        if_entry.condition.clone()
                    };
                    self.collect_items(
                        &if_entry.entries,
                        depth,
                        parent_id,
                        items,
                        Some(&combined_condition),
                    );
                }
                Entry::MainMenu(_title) => {
                    // Skip mainmenu for now
                }
                Entry::Source(_) => {
                    // Source entries are handled during parsing
                }
            }
        }
    }

    /// Combine two expressions with AND logic
    /// If existing_depends is None, returns new_condition
    /// If existing_depends is Some, returns (existing_depends AND new_condition)
    fn combine_conditions(existing_depends: Option<&Expr>, new_condition: &Expr) -> Expr {
        match existing_depends {
            None => new_condition.clone(),
            Some(existing) => {
                Expr::And(Box::new(existing.clone()), Box::new(new_condition.clone()))
            }
        }
    }

    pub fn get_items_for_path(&self, path: &[String]) -> Vec<MenuItem> {
        let key = if path.is_empty() {
            "root".to_string()
        } else {
            path.last().unwrap().clone()
        };

        debug_log!("[GET_ITEMS] Called with path={:?}, key='{}'", path, key);

        let items = self.menu_tree.get(&key).cloned().unwrap_or_else(|| {
            debug_log!("    ❌ Key '{}' not found in menu_tree", key);
            debug_log!(
                "    Available keys: {:?}",
                self.menu_tree.keys().collect::<Vec<_>>()
            );
            Vec::new()
        });

        debug_log!("    ✅ Returning {} items for key '{}'", items.len(), key);
        items
    }

    fn build_reverse_dependencies(&mut self) {
        // Build maps of reverse dependencies
        let mut selected_by_map: HashMap<String, Vec<String>> = HashMap::new();
        let mut implied_by_map: HashMap<String, Vec<String>> = HashMap::new();

        // First pass: collect all reverse dependencies
        for item in &self.all_items {
            for select in &item.selects {
                selected_by_map
                    .entry(select.clone())
                    .or_insert_with(Vec::new)
                    .push(item.id.clone());
            }
            for imply in &item.implies {
                implied_by_map
                    .entry(imply.clone())
                    .or_insert_with(Vec::new)
                    .push(item.id.clone());
            }
        }

        // Second pass: update all items with reverse dependencies
        for item in &mut self.all_items {
            if let Some(selected_by) = selected_by_map.get(&item.id) {
                item.selected_by = selected_by.clone();
            }
            if let Some(implied_by) = implied_by_map.get(&item.id) {
                item.implied_by = implied_by.clone();
            }
        }

        // Third pass: update menu_tree items with reverse dependencies
        for (_, items) in self.menu_tree.iter_mut() {
            for item in items {
                if let Some(selected_by) = selected_by_map.get(&item.id) {
                    item.selected_by = selected_by.clone();
                }
                if let Some(implied_by) = implied_by_map.get(&item.id) {
                    item.implied_by = implied_by.clone();
                }
            }
        }
    }
}

impl Default for ConfigState {
    fn default() -> Self {
        Self::new()
    }
}
