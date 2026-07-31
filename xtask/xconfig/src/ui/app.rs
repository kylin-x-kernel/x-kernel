// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{io::Write, time::Duration};

use crossterm::event::{self, Event, KeyCode, KeyEvent};
use ratatui::{
    Frame, Terminal,
    backend::Backend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::{
    config::ConfigEngine,
    debug_log,
    error::Result,
    kconfig::{Expr, SymbolTable, SymbolType},
    ui::{
        dependency_resolver::{DependencyError, DependencyResolver},
        events::EventResult,
        rendering::Theme,
        state::{ConfigState, ConfigValue, MenuItem, MenuItemKind, NavigationState, TristateValue},
        utils::FuzzySearcher,
    },
};
/// Maximum number of dependency violations to display in error dialog
const MAX_DISPLAYED_VIOLATIONS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelFocus {
    MenuTree,
    SearchBar,
    Dialog,
}

#[derive(Debug, Clone)]
pub enum DialogType {
    Help,
    Save,
    DependencyError(DependencyError),
    CascadeWarning {
        symbol: String,
        affected: Vec<String>,
    },
    ImplySuggestion {
        implied: Vec<String>,
    },
    EditString {
        symbol: String,
        current_value: String,
        prompt: String,
    },
    EditInt {
        symbol: String,
        current_value: i64,
        prompt: String,
    },
    EditHex {
        symbol: String,
        current_value: String,
        prompt: String,
    },
    EditRange {
        symbol: String,
        current_value: String,
        prompt: String,
    },
}

pub struct MenuConfigApp {
    config_state: ConfigState,
    engine: ConfigEngine,
    navigation: NavigationState,

    // Search state
    search_active: bool,
    search_query: String,

    // UI state
    focus: PanelFocus,
    dialog_type: Option<DialogType>,

    // Theme
    theme: Theme,

    // Status message
    status_message: Option<String>,

    // Input state for editing
    input_buffer: String,
    input_cursor: usize,
}

impl MenuConfigApp {
    pub fn new(
        entries: Vec<crate::kconfig::ast::Entry>,
        symbol_table: SymbolTable,
    ) -> Result<Self> {
        let mut dependency_resolver = DependencyResolver::new();
        dependency_resolver.build_from_entries(&entries);
        Self::new_with_resolver(entries, symbol_table, dependency_resolver)
    }

    pub fn new_with_resolver(
        entries: Vec<crate::kconfig::ast::Entry>,
        symbol_table: SymbolTable,
        dependency_resolver: DependencyResolver,
    ) -> Result<Self> {
        let mut config_state = ConfigState::build_from_entries(&entries);

        // Initialize values from symbol table
        for item in &mut config_state.all_items {
            if let MenuItemKind::Config { symbol_type } | MenuItemKind::MenuConfig { symbol_type } =
                &item.kind
            {
                let symbol_type = symbol_type.clone();
                let had_value = Self::initialize_item_value(item, &symbol_type, &symbol_table);
                // Store original value for tracking modifications
                if had_value
                    && let Some(value) = symbol_table.get_value(&item.id) {
                        config_state
                            .original_values
                            .insert(item.id.clone(), value.clone());
                    }
            }
        }

        // Also initialize values in menu_tree (critical fix for checkbox display)
        for (_, items) in config_state.menu_tree.iter_mut() {
            for item in items {
                if let MenuItemKind::Config { symbol_type }
                | MenuItemKind::MenuConfig { symbol_type } = &item.kind
                {
                    let symbol_type = symbol_type.clone();
                    Self::initialize_item_value(item, &symbol_type, &symbol_table);
                }
            }
        }

        Ok(Self {
            config_state,
            engine: ConfigEngine::from_parts(entries, symbol_table, dependency_resolver),
            navigation: NavigationState::new(),
            search_active: false,
            search_query: String::new(),
            focus: PanelFocus::MenuTree,
            dialog_type: None,
            theme: Theme::default(),
            status_message: None,
            input_buffer: String::new(),
            input_cursor: 0,
        })
    }

    /// Initialize the value for a menu item from the symbol table or set a default value.
    ///
    /// This method looks up the item's value in the symbol table and updates the item's value field.
    /// If no value is found in the symbol table, it sets a default value based on the symbol type.
    ///
    /// # Arguments
    /// * `item` - The menu item to initialize
    /// * `symbol_type` - The type of the symbol (Bool, Tristate, String, Int, or Hex)
    /// * `symbol_table` - The symbol table containing configuration values
    ///
    /// # Returns
    /// `true` if a value was found in the symbol table, `false` if a default was used
    fn initialize_item_value(
        item: &mut MenuItem,
        symbol_type: &SymbolType,
        symbol_table: &SymbolTable,
    ) -> bool {
        if let Some(value) = symbol_table.get_value(&item.id) {
            item.value = Some(Self::parse_value(&value, symbol_type));
            true
        } else {
            // Set default value based on type
            let default_val = match symbol_type {
                SymbolType::Bool => ConfigValue::Bool(false),
                SymbolType::Tristate => ConfigValue::Tristate(TristateValue::No),
                SymbolType::String => ConfigValue::String(String::new()),
                SymbolType::U8
                | SymbolType::U16
                | SymbolType::U32
                | SymbolType::U64
                | SymbolType::U128
                | SymbolType::Usize
                | SymbolType::I8
                | SymbolType::I16
                | SymbolType::I32
                | SymbolType::I64
                | SymbolType::I128
                | SymbolType::Isize => ConfigValue::Int(0),
                SymbolType::Hex => ConfigValue::Hex("0x0".to_string()),
                SymbolType::Range(_) => ConfigValue::Range("[]".to_string()),
            };
            item.value = Some(default_val);
            false
        }
    }

    fn parse_value(value: &str, symbol_type: &SymbolType) -> ConfigValue {
        match symbol_type {
            SymbolType::Bool => ConfigValue::Bool(value == "y"),
            SymbolType::Tristate => match value {
                "y" => ConfigValue::Tristate(TristateValue::Yes),
                "m" => ConfigValue::Tristate(TristateValue::Module),
                _ => ConfigValue::Tristate(TristateValue::No),
            },
            SymbolType::String => ConfigValue::String(value.trim_matches('"').to_string()),
            SymbolType::U8
            | SymbolType::U16
            | SymbolType::U32
            | SymbolType::U64
            | SymbolType::U128
            | SymbolType::Usize
            | SymbolType::I8
            | SymbolType::I16
            | SymbolType::I32
            | SymbolType::I64
            | SymbolType::I128
            | SymbolType::Isize => ConfigValue::Int(value.parse().unwrap_or(0)),
            SymbolType::Hex => {
                let trimmed = value.trim();
                // If already in hex format, normalize to lowercase
                if let Some(hex_part) = trimmed
                    .strip_prefix("0x")
                    .or_else(|| trimmed.strip_prefix("0X"))
                {
                    ConfigValue::Hex(format!("0x{}", hex_part.to_lowercase()))
                } else {
                    // If it's a decimal integer, convert to hex
                    match trimmed.parse::<i64>() {
                        Ok(num) if num >= 0 => ConfigValue::Hex(format!("0x{:x}", num)),
                        Ok(num) => {
                            // Use unsigned_abs to avoid overflow for i64::MIN
                            let abs_val = num.unsigned_abs();
                            ConfigValue::Hex(format!("-0x{:x}", abs_val))
                        }
                        Err(_) => ConfigValue::Hex(trimmed.to_string()), // Keep as-is if invalid
                    }
                }
            }
            SymbolType::Range(_) => {
                let trimmed = value.trim();
                if trimmed.starts_with('[') && trimmed.ends_with(']') {
                    ConfigValue::Range(trimmed.to_string())
                } else {
                    ConfigValue::Range(format!("[{}]", trimmed))
                }
            }
        }
    }

    /// Get the current visible items list (with filtering applied)
    /// This ensures consistent behavior across all navigation methods
    fn get_visible_items(&self) -> Vec<MenuItem> {
        let items = if self.search_active && !self.search_query.is_empty() {
            let searcher = FuzzySearcher::new(self.search_query.clone());
            let results = searcher.search(&self.config_state.all_items);
            results.into_iter().map(|r| r.item).collect::<Vec<_>>()
        } else {
            self.config_state
                .get_items_for_path(&self.navigation.current_path)
        };
        self.filter_visible_items(items)
    }

    /// Filter menu items based on visibility rules:
    /// 1. Items without prompts are hidden (internal variables)
    /// 2. Items with unsatisfied depends_on conditions are hidden
    pub fn filter_visible_items(&self, items: Vec<MenuItem>) -> Vec<MenuItem> {
        use crate::ui::dependency_resolver::ExprEvaluator;
        let evaluator = ExprEvaluator::new();

        items
            .into_iter()
            .filter(|item| {
                // Rule 1: Config/MenuConfig items without prompts are never shown
                match &item.kind {
                    MenuItemKind::Config { .. } | MenuItemKind::MenuConfig { .. }
                        if !item.has_prompt => {
                            return false;
                        }
                    MenuItemKind::Choice { .. }
                        if !item.has_prompt => {
                            return false;
                        }
                    _ => {} // Menus and Comments are always visible if dependencies are met
                }

                // Rule 2: Check depends_on condition
                if let Some(depends_expr) = &item.depends_on {
                    return evaluator.evaluate(depends_expr, self.engine.symbols());
                }

                true
            })
            .collect()
    }

    /// Get reference to config_state (for testing)
    pub fn config_state(&self) -> &ConfigState {
        &self.config_state
    }

    #[cfg(test)]
    pub fn engine(&self) -> &ConfigEngine {
        &self.engine
    }

    pub fn run<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<()> {
        loop {
            terminal.draw(|f| self.render(f))?;

            if event::poll(Duration::from_millis(100))?
                && let Event::Key(key) = event::read()? {
                    match self.handle_key(key)? {
                        EventResult::Quit => break,
                        EventResult::Continue => {}
                    }
                }
        }

        Ok(())
    }

    fn render(&mut self, frame: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Header
                Constraint::Length(3), // Search bar
                Constraint::Min(0),    // Main content
                Constraint::Length(3), // Status bar
            ])
            .split(frame.size());

        self.render_header(frame, chunks[0]);
        self.render_search_bar(frame, chunks[1]);
        self.render_main_content(frame, chunks[2]);
        self.render_status_bar(frame, chunks[3]);

        // Render dialogs
        if let Some(dialog) = &self.dialog_type {
            match dialog {
                DialogType::Help => self.render_help_modal(frame),
                DialogType::Save => self.render_save_dialog(frame),
                DialogType::DependencyError(error) => {
                    self.render_dependency_error_dialog(frame, error)
                }
                DialogType::CascadeWarning { symbol, affected } => {
                    self.render_cascade_warning_dialog(frame, symbol, affected)
                }
                DialogType::ImplySuggestion { implied } => {
                    self.render_imply_suggestion_dialog(frame, implied)
                }
                DialogType::EditString { symbol, prompt, .. } => {
                    self.render_input_dialog(
                        frame,
                        prompt,
                        symbol,
                        "String",
                        "Enter text and press Enter to save",
                    );
                }
                DialogType::EditInt { symbol, prompt, .. } => {
                    self.render_input_dialog(
                        frame,
                        prompt,
                        symbol,
                        "Integer",
                        "Enter a number (e.g., 123, -456)",
                    );
                }
                DialogType::EditHex { symbol, prompt, .. } => {
                    self.render_input_dialog(
                        frame,
                        prompt,
                        symbol,
                        "Hexadecimal",
                        "Enter hex value (e.g., 0xFF, 0x1A2B)",
                    );
                }
                DialogType::EditRange { symbol, prompt, .. } => {
                    self.render_input_dialog(
                        frame,
                        prompt,
                        symbol,
                        "Range Array",
                        "Enter array values: [item1, item2, ...] (e.g., [0x0, 0x1] or [foo, bar])",
                    );
                }
            }
        }
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let modified_count = self.config_state.modified_symbols.len();
        let title = format!(
            " 🔧 Rust Kbuild Configuration{}{}",
            if modified_count > 0 {
                format!("  Changed: {}", modified_count)
            } else {
                String::new()
            },
            "  [S]ave [Q]uit "
        );

        let header = Paragraph::new(title)
            .style(self.theme.get_info_style().add_modifier(Modifier::BOLD))
            .block(Block::default().borders(Borders::ALL));

        frame.render_widget(header, area);
    }

    fn render_search_bar(&self, frame: &mut Frame, area: Rect) {
        let search_text = if self.search_active {
            format!(" 🔍 Search: {}_", self.search_query)
        } else {
            " 🔍 Press / to search".to_string()
        };

        let style = if self.search_active {
            self.theme.get_selected_style()
        } else {
            Style::default()
        };

        let search = Paragraph::new(search_text)
            .style(style)
            .block(Block::default().borders(Borders::ALL));

        frame.render_widget(search, area);
    }

    fn render_main_content(&mut self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(area);

        self.render_menu_tree(frame, chunks[0]);
        self.render_detail_panel(frame, chunks[1]);
    }

    fn render_menu_tree(&mut self, frame: &mut Frame, area: Rect) {
        let visible_items = self.get_visible_items();
        if visible_items.is_empty() {
            let empty = Paragraph::new("No items found").block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Configuration Menu "),
            );
            frame.render_widget(empty, area);
            return;
        }

        // Ensure selected index is valid
        if self.navigation.selected_index >= visible_items.len() {
            self.navigation.selected_index = visible_items.len().saturating_sub(1);
        }

        let list_items: Vec<ListItem> = visible_items
            .iter()
            .enumerate()
            .map(|(idx, item)| {
                let is_selected = idx == self.navigation.selected_index;
                self.create_list_item(item, is_selected)
            })
            .collect();

        let list = List::new(list_items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Configuration Menu ")
                .border_style(if self.focus == PanelFocus::MenuTree {
                    self.theme.get_selected_style()
                } else {
                    self.theme.get_border_style()
                }),
        );

        // Use a stateful list so the viewport auto-scrolls to keep the
        // selected item visible when the menu is taller than the area.
        let mut list_state = ListState::default();
        list_state = list_state.with_offset(self.navigation.scroll_offset);
        list_state.select(Some(self.navigation.selected_index));
        frame.render_stateful_widget(list, area, &mut list_state);
        self.navigation.scroll_offset = list_state.offset();
    }

    fn create_list_item(&self, item: &MenuItem, is_selected: bool) -> ListItem<'_> {
        let indent = "  ".repeat(item.depth);
        let icon = self.get_item_icon(item);
        let checkbox = self.get_checkbox_symbol(item);
        let label = &item.label;
        let value_display = self.format_value_display(item);

        let style = if is_selected {
            self.theme.get_selected_style()
        } else if !item.is_enabled {
            self.theme.get_disabled_style()
        } else {
            Style::default()
        };

        let text = format!(
            "{}{} {} {} {}",
            indent, icon, checkbox, label, value_display
        );
        ListItem::new(text).style(style)
    }

    fn get_item_icon(&self, item: &MenuItem) -> &str {
        match &item.kind {
            MenuItemKind::Menu { .. } => {
                if item.has_children {
                    "📁"
                } else {
                    "📂"
                }
            }
            MenuItemKind::Config { .. } | MenuItemKind::MenuConfig { .. } => "⚙️ ",
            MenuItemKind::Choice { .. } => "◉",
            MenuItemKind::Comment { .. } => "💬",
        }
    }

    fn get_checkbox_symbol(&self, item: &MenuItem) -> &str {
        match &item.value {
            Some(ConfigValue::Bool(true)) => "[✓]",
            Some(ConfigValue::Bool(false)) => "[ ]",
            Some(ConfigValue::Tristate(TristateValue::Yes)) => "[✓]",
            Some(ConfigValue::Tristate(TristateValue::No)) => "[ ]",
            Some(ConfigValue::Tristate(TristateValue::Module)) => "[M]",
            None if !item.is_enabled => "[✗]",
            _ => "   ",
        }
    }

    fn format_value_display(&self, item: &MenuItem) -> String {
        match &item.value {
            Some(ConfigValue::String(s)) if !s.is_empty() => format!("= \"{}\"", s),
            Some(ConfigValue::Int(i)) => format!("= {}", i),
            Some(ConfigValue::Hex(h)) => format!("= {}", h),
            Some(ConfigValue::Range(r)) => format!("= {}", r),
            _ => String::new(),
        }
    }

    fn render_detail_panel(&self, frame: &mut Frame, area: Rect) {
        let visible_items = self.get_visible_items();

        if visible_items.is_empty() || self.navigation.selected_index >= visible_items.len() {
            let empty = Paragraph::new("No item selected").block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" 📖 Help & Details "),
            );
            frame.render_widget(empty, area);
            return;
        }

        let item = &visible_items[self.navigation.selected_index];

        let mut text_lines = vec![];

        // Title
        text_lines.push(Line::from(vec![
            Span::styled("📖 ", self.theme.get_info_style()),
            Span::styled(&item.label, Style::default().add_modifier(Modifier::BOLD)),
        ]));
        text_lines.push(Line::from(""));

        // Type and ID
        let type_str = match &item.kind {
            MenuItemKind::Config { symbol_type } | MenuItemKind::MenuConfig { symbol_type } => {
                format!("Type: {:?}", symbol_type)
            }
            MenuItemKind::Menu { .. } => "Type: Menu".to_string(),
            MenuItemKind::Choice { .. } => "Type: Choice".to_string(),
            MenuItemKind::Comment { .. } => "Type: Comment".to_string(),
        };
        text_lines.push(Line::from(type_str));
        text_lines.push(Line::from(format!("ID: {}", item.id)));
        text_lines.push(Line::from(""));

        // Current value
        if let Some(value) = &item.value {
            let value_str = match value {
                ConfigValue::Bool(true) => "Status: ✓ Enabled".to_string(),
                ConfigValue::Bool(false) => "Status: Disabled".to_string(),
                ConfigValue::Tristate(TristateValue::Yes) => "Status: ✓ Yes".to_string(),
                ConfigValue::Tristate(TristateValue::No) => "Status: No".to_string(),
                ConfigValue::Tristate(TristateValue::Module) => "Status: Module".to_string(),
                ConfigValue::String(s) => format!("Value: \"{}\"", s),
                ConfigValue::Int(i) => format!("Value: {}", i),
                ConfigValue::Hex(h) => format!("Value: {}", h),
                ConfigValue::Range(r) => format!("Value: {}", r),
            };
            text_lines.push(Line::from(value_str));
            text_lines.push(Line::from(""));
        }

        // Help text
        if let Some(help) = &item.help_text {
            text_lines.push(Line::from("Description:"));
            text_lines.push(Line::from("━━━━━━━━━━━━"));
            // Split help text into lines
            for line in help.lines() {
                text_lines.push(Line::from(line.to_string()));
            }
            text_lines.push(Line::from(""));
        }

        // Dependencies
        if !item.selects.is_empty() {
            text_lines.push(Line::from("⚡ Enables:"));
            for select in &item.selects {
                text_lines.push(Line::from(format!("  • {}", select)));
            }
            text_lines.push(Line::from(""));
        }

        // Depends on section
        if let Some(depends) = &item.depends_on {
            text_lines.push(Line::from("🔗 Depends on:"));
            text_lines.push(Line::from(format!("  {}", Self::format_expr(depends))));
            text_lines.push(Line::from(""));
        }

        // Selected by section
        if !item.selected_by.is_empty() {
            text_lines.push(Line::from("⬆️  Selected by:"));
            for sel_by in &item.selected_by {
                text_lines.push(Line::from(format!("  • {}", sel_by)));
            }
            text_lines.push(Line::from(""));
        }

        // Implied by section
        if !item.implied_by.is_empty() {
            text_lines.push(Line::from("💡 Implied by:"));
            for impl_by in &item.implied_by {
                text_lines.push(Line::from(format!("  • {}", impl_by)));
            }
        }

        let detail = Paragraph::new(text_lines).wrap(Wrap { trim: true }).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" 📖 Help & Details "),
        );

        frame.render_widget(detail, area);
    }

    fn render_status_bar(&self, frame: &mut Frame, area: Rect) {
        let status_text = if let Some(msg) = &self.status_message {
            msg.clone()
        } else {
            " ↑↓:Navigate │ Space:Toggle │ Enter:Open │ /:Search │ ?:Help │ ESC:Back".to_string()
        };

        let status = Paragraph::new(status_text).block(Block::default().borders(Borders::ALL));

        frame.render_widget(status, area);
    }

    fn render_help_modal(&self, frame: &mut Frame) {
        let area = self.centered_rect(60, 70, frame.size());

        let help_text = vec![
            "Keyboard Shortcuts",
            "══════════════════",
            "",
            "Navigation:",
            "  ↑/k        - Move up",
            "  ↓/j        - Move down",
            "  ←/h/ESC    - Go back",
            "  →/l/Enter  - Enter submenu",
            "  PageUp     - Page up",
            "  PageDown   - Page down",
            "  Home       - Jump to first",
            "  End        - Jump to last",
            "",
            "Actions:",
            "  Space      - Toggle option",
            "  s/S        - Save configuration",
            "  q/Q        - Quit",
            "  /          - Search",
            "  ?          - Show this help",
            "",
            "Press any key to close",
        ];

        let text: Vec<Line> = help_text.into_iter().map(Line::from).collect();

        let help = Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Help ")
                .style(self.theme.get_info_style()),
        );

        frame.render_widget(help, area);
    }

    fn render_save_dialog(&self, frame: &mut Frame) {
        let area = self.centered_rect(50, 30, frame.size());

        let text = vec![
            "Save Configuration?",
            "",
            "You have unsaved changes.",
            "",
            "  y - Save and quit",
            "  n - Quit without saving",
            "  ESC - Cancel",
        ];

        let lines: Vec<Line> = text.into_iter().map(Line::from).collect();

        let dialog = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Confirm ")
                .style(self.theme.get_warning_style()),
        );

        frame.render_widget(dialog, area);
    }

    fn centered_rect(&self, percent_x: u16, percent_y: u16, r: Rect) -> Rect {
        let popup_layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage((100 - percent_y) / 2),
                Constraint::Percentage(percent_y),
                Constraint::Percentage((100 - percent_y) / 2),
            ])
            .split(r);

        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage((100 - percent_x) / 2),
                Constraint::Percentage(percent_x),
                Constraint::Percentage((100 - percent_x) / 2),
            ])
            .split(popup_layout[1])[1]
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<EventResult> {
        debug_log!(
            "⌨️  [handle_key] key={:?}, current_path={:?}",
            key.code,
            self.navigation.current_path
        );

        // Handle dialogs first - check type without moving
        let has_dialog = self.dialog_type.is_some();
        if has_dialog {
            return match &self.dialog_type {
                Some(DialogType::Help) => {
                    self.dialog_type = None;
                    Ok(EventResult::Continue)
                }
                Some(DialogType::Save) => self.handle_save_dialog_key(key),
                Some(DialogType::DependencyError(_)) => {
                    self.handle_dependency_error_dialog_key(key)
                }
                Some(DialogType::CascadeWarning { .. }) => {
                    self.handle_cascade_warning_dialog_key(key)
                }
                Some(DialogType::ImplySuggestion { .. }) => {
                    self.handle_imply_suggestion_dialog_key(key)
                }
                Some(DialogType::EditString { .. })
                | Some(DialogType::EditInt { .. })
                | Some(DialogType::EditHex { .. })
                | Some(DialogType::EditRange { .. }) => self.handle_input_dialog_key(key),
                None => Ok(EventResult::Continue),
            };
        }

        // Handle search mode
        if self.search_active {
            return self.handle_search_key(key);
        }

        // Main navigation
        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => {
                if !self.config_state.modified_symbols.is_empty() {
                    self.dialog_type = Some(DialogType::Save);
                    Ok(EventResult::Continue)
                } else {
                    Ok(EventResult::Quit)
                }
            }
            KeyCode::Char('s') | KeyCode::Char('S') => {
                self.save_config()?;
                Ok(EventResult::Continue)
            }
            KeyCode::Char('?') => {
                self.dialog_type = Some(DialogType::Help);
                Ok(EventResult::Continue)
            }
            KeyCode::Char('/') => {
                self.search_active = true;
                self.search_query.clear();
                self.focus = PanelFocus::SearchBar;
                Ok(EventResult::Continue)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_up();
                Ok(EventResult::Continue)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_down();
                Ok(EventResult::Continue)
            }
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Esc => {
                self.go_back();
                Ok(EventResult::Continue)
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => {
                self.enter_submenu();
                Ok(EventResult::Continue)
            }
            KeyCode::Char(' ') => {
                self.toggle_current_item()?;
                Ok(EventResult::Continue)
            }
            KeyCode::PageUp => {
                self.page_up();
                Ok(EventResult::Continue)
            }
            KeyCode::PageDown => {
                self.page_down();
                Ok(EventResult::Continue)
            }
            KeyCode::Home => {
                self.jump_to_first();
                Ok(EventResult::Continue)
            }
            KeyCode::End => {
                self.jump_to_last();
                Ok(EventResult::Continue)
            }
            _ => Ok(EventResult::Continue),
        }
    }

    fn handle_save_dialog_key(&mut self, key: KeyEvent) -> Result<EventResult> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.save_config()?;
                self.dialog_type = None;
                Ok(EventResult::Quit)
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                self.dialog_type = None;
                Ok(EventResult::Quit)
            }
            KeyCode::Esc => {
                self.dialog_type = None;
                Ok(EventResult::Continue)
            }
            _ => Ok(EventResult::Continue),
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> Result<EventResult> {
        match key.code {
            KeyCode::Esc => {
                self.search_active = false;
                self.search_query.clear();
                self.focus = PanelFocus::MenuTree;
                self.navigation.selected_index = 0;
                Ok(EventResult::Continue)
            }
            KeyCode::Enter => {
                // Get the currently selected item from search results
                let mut navigated = false;
                if !self.search_query.is_empty() {
                    let searcher = FuzzySearcher::new(self.search_query.clone());
                    let results = searcher.search(&self.config_state.all_items);

                    if !results.is_empty() && self.navigation.selected_index < results.len() {
                        let selected_item = &results[self.navigation.selected_index].item;
                        let item_label = selected_item.label.clone();
                        let item_id = selected_item.id.clone();

                        // Find the item's location in the menu tree
                        if let Some((path, index)) = self.find_item_location(&item_id) {
                            // Navigate to the item's location
                            self.navigation.current_path = path;
                            self.navigation.selected_index = index;
                            self.navigation.scroll_offset = 0;
                            // The position stack only makes sense for the
                            // enter/back path; jumping to an arbitrary item
                            // invalidates it.
                            self.navigation.position_stack.clear();
                            self.status_message = Some(format!(" Jumped to {}", item_label));
                            navigated = true;
                        }
                    }
                }

                // Exit search mode and clear query only if navigation was successful or Enter was pressed with results
                if navigated || !self.search_query.is_empty() {
                    self.search_active = false;
                    self.search_query.clear();
                    self.focus = PanelFocus::MenuTree;
                }
                Ok(EventResult::Continue)
            }
            KeyCode::Backspace => {
                self.search_query.pop();
                self.navigation.selected_index = 0;
                Ok(EventResult::Continue)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                // Allow navigation in search results
                if self.navigation.selected_index > 0 {
                    self.navigation.selected_index -= 1;
                }
                Ok(EventResult::Continue)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                // Allow navigation in search results
                let searcher = FuzzySearcher::new(self.search_query.clone());
                let results = searcher.search(&self.config_state.all_items);
                if !results.is_empty() && self.navigation.selected_index < results.len() - 1 {
                    self.navigation.selected_index += 1;
                }
                Ok(EventResult::Continue)
            }
            KeyCode::Char(c) => {
                self.search_query.push(c);
                self.navigation.selected_index = 0;
                Ok(EventResult::Continue)
            }
            _ => Ok(EventResult::Continue),
        }
    }

    fn handle_dependency_error_dialog_key(&mut self, key: KeyEvent) -> Result<EventResult> {
        match key.code {
            KeyCode::Esc => {
                self.dialog_type = None;
                Ok(EventResult::Continue)
            }
            _ => Ok(EventResult::Continue),
        }
    }

    fn handle_cascade_warning_dialog_key(&mut self, key: KeyEvent) -> Result<EventResult> {
        // Extract dialog data before any mutable operations
        let (symbol, affected) =
            if let Some(DialogType::CascadeWarning { symbol, affected }) = &self.dialog_type {
                (symbol.clone(), affected.clone())
            } else {
                return Ok(EventResult::Continue);
            };

        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                // Disable affected dependents first, then the requested symbol.
                for affected_symbol in &affected {
                    self.apply_value_change(affected_symbol, ConfigValue::Bool(false))?;
                }
                self.apply_value_change(&symbol, ConfigValue::Bool(false))?;
                self.sync_ui_state_from_symbol_table()?;
                self.update_enabled_states()?;
                self.status_message = if affected.is_empty() {
                    Some(format!(" {} disabled", symbol))
                } else {
                    Some(format!(
                        " {} disabled (also disabled: {})",
                        symbol,
                        affected.join(", ")
                    ))
                };
                self.dialog_type = None;
                Ok(EventResult::Continue)
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.dialog_type = None;
                Ok(EventResult::Continue)
            }
            _ => Ok(EventResult::Continue),
        }
    }

    fn handle_imply_suggestion_dialog_key(&mut self, key: KeyEvent) -> Result<EventResult> {
        // Extract implied list before any mutable operations
        let implied = if let Some(DialogType::ImplySuggestion { implied }) = &self.dialog_type {
            implied.clone()
        } else {
            return Ok(EventResult::Continue);
        };

        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                // Enable implied symbols
                for symbol in &implied {
                    self.engine.set_value(symbol, "y".to_string());
                }
                self.sync_ui_state_from_symbol_table()?;
                self.update_enabled_states()?;
                self.status_message = Some(format!(" Enabled: {}", implied.join(", ")));
                self.dialog_type = None;
                Ok(EventResult::Continue)
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.dialog_type = None;
                Ok(EventResult::Continue)
            }
            _ => Ok(EventResult::Continue),
        }
    }

    fn handle_input_dialog_key(&mut self, key: KeyEvent) -> Result<EventResult> {
        match key.code {
            KeyCode::Char(c) => {
                // Filter input based on dialog type
                if let Some(DialogType::EditInt { .. }) = &self.dialog_type {
                    // For integers, only allow digits and minus sign at position 0
                    if c == '-' {
                        if self.input_cursor != 0 {
                            return Ok(EventResult::Continue);
                        }
                    } else if !c.is_ascii_digit() {
                        return Ok(EventResult::Continue);
                    }
                } else if let Some(DialogType::EditHex { .. }) = &self.dialog_type
                    && !c.is_ascii_hexdigit() && c != 'x' && c != 'X' {
                        return Ok(EventResult::Continue);
                    }

                self.input_buffer.insert(self.input_cursor, c);
                self.input_cursor += 1;
                Ok(EventResult::Continue)
            }
            KeyCode::Backspace => {
                if self.input_cursor > 0 {
                    self.input_cursor -= 1;
                    self.input_buffer.remove(self.input_cursor);
                }
                Ok(EventResult::Continue)
            }
            KeyCode::Delete => {
                if self.input_cursor < self.input_buffer.len() {
                    self.input_buffer.remove(self.input_cursor);
                }
                Ok(EventResult::Continue)
            }
            KeyCode::Left => {
                if self.input_cursor > 0 {
                    self.input_cursor -= 1;
                }
                Ok(EventResult::Continue)
            }
            KeyCode::Right => {
                if self.input_cursor < self.input_buffer.len() {
                    self.input_cursor += 1;
                }
                Ok(EventResult::Continue)
            }
            KeyCode::Home => {
                self.input_cursor = 0;
                Ok(EventResult::Continue)
            }
            KeyCode::End => {
                self.input_cursor = self.input_buffer.len();
                Ok(EventResult::Continue)
            }
            KeyCode::Enter => {
                self.save_input_dialog()?;
                Ok(EventResult::Continue)
            }
            KeyCode::Esc => {
                self.dialog_type = None;
                self.focus = PanelFocus::MenuTree;
                self.input_buffer.clear();
                self.status_message = Some("✗ Edit cancelled".to_string());
                Ok(EventResult::Continue)
            }
            _ => Ok(EventResult::Continue),
        }
    }

    fn move_up(&mut self) {
        if self.navigation.selected_index > 0 {
            self.navigation.selected_index -= 1;
        }
    }

    fn move_down(&mut self) {
        let visible_items = self.get_visible_items();

        if !visible_items.is_empty() && self.navigation.selected_index < visible_items.len() - 1 {
            self.navigation.selected_index += 1;
        }
    }

    fn enter_submenu(&mut self) {
        let visible_items = self.get_visible_items();

        if visible_items.is_empty() || self.navigation.selected_index >= visible_items.len() {
            return;
        }

        let item = &visible_items[self.navigation.selected_index];
        debug_log!(
            "    [3] Selected item: id='{}', label='{}', has_children={}",
            item.id,
            item.label,
            item.has_children
        );

        debug_log!("🚪 Attempting to enter submenu:");
        debug_log!("    item.id: '{}'", item.id);
        debug_log!("    item.label: '{}'", item.label);
        debug_log!("    item.has_children: {}", item.has_children);
        debug_log!(
            "    current_path before: {:?}",
            self.navigation.current_path
        );

        if item.has_children {
            // Remember where we were in the parent menu so `go_back` can
            // restore the selection instead of resetting to the top.
            self.navigation.position_stack.push((
                self.navigation.selected_index,
                self.navigation.scroll_offset,
            ));
            self.navigation.current_path.push(item.id.clone());

            debug_log!("    ✅ Entering submenu");
            debug_log!("    current_path after: {:?}", self.navigation.current_path);

            self.navigation.selected_index = 0;
            self.navigation.scroll_offset = 0;

            debug_log!("🚪 [END] enter_submenu finished");
        } else {
            debug_log!("    ❌ Item has no children, cannot enter submenu");
        }
    }

    fn go_back(&mut self) {
        debug_log!(
            "⬅️ [go_back] Called, current_path before: {:?}",
            self.navigation.current_path
        );
        if !self.navigation.current_path.is_empty() {
            self.navigation.current_path.pop();
            debug_log!(
                "    ✅ Popped, current_path after: {:?}",
                self.navigation.current_path
            );
            // Restore the selection we had before entering this submenu.
            let (selected, offset) = self.navigation.position_stack.pop().unwrap_or((0, 0));
            self.navigation.selected_index = selected;
            self.navigation.scroll_offset = offset;
        } else {
            debug_log!("    ❌ Already at root, cannot go back");
        }
    }

    fn page_up(&mut self) {
        let visible_items = self.get_visible_items();
        if !visible_items.is_empty() {
            self.navigation.selected_index = self.navigation.selected_index.saturating_sub(10);
            // Ensure we don't go beyond the list
            if self.navigation.selected_index >= visible_items.len() {
                self.navigation.selected_index = visible_items.len().saturating_sub(1);
            }
        }
    }

    fn page_down(&mut self) {
        let visible_items = self.get_visible_items();

        if !visible_items.is_empty() {
            self.navigation.selected_index =
                (self.navigation.selected_index + 10).min(visible_items.len() - 1);
        }
    }

    fn jump_to_first(&mut self) {
        self.navigation.selected_index = 0;
    }

    fn jump_to_last(&mut self) {
        let items = if self.search_active && !self.search_query.is_empty() {
            let searcher = FuzzySearcher::new(self.search_query.clone());
            let results = searcher.search(&self.config_state.all_items);
            results.into_iter().map(|r| r.item).collect::<Vec<_>>()
        } else {
            self.config_state
                .get_items_for_path(&self.navigation.current_path)
        };

        // Apply visibility filtering
        let visible_items = self.filter_visible_items(items);

        if !visible_items.is_empty() {
            self.navigation.selected_index = visible_items.len() - 1;
        }
    }

    fn toggle_current_item(&mut self) -> Result<()> {
        let visible_items = self.get_visible_items();

        if visible_items.is_empty() || self.navigation.selected_index >= visible_items.len() {
            return Ok(());
        }

        let item = &visible_items[self.navigation.selected_index];
        let item_id = item.id.clone();

        // Check if this is a choice option
        if let Some(parent_choice_id) = &item.parent_choice {
            return self.handle_choice_selection(parent_choice_id, &item_id);
        }

        // Check if this is a string/int/hex config item that needs editing
        if let MenuItemKind::Config { symbol_type } | MenuItemKind::MenuConfig { symbol_type } =
            &item.kind
        {
            match symbol_type {
                SymbolType::String => {
                    let current = match &item.value {
                        Some(ConfigValue::String(s)) => s.clone(),
                        _ => String::new(),
                    };
                    self.dialog_type = Some(DialogType::EditString {
                        symbol: item.id.clone(),
                        current_value: current.clone(),
                        prompt: item.label.clone(),
                    });
                    self.input_buffer = current;
                    self.input_cursor = self.input_buffer.len();
                    self.focus = PanelFocus::Dialog;
                    return Ok(());
                }
                ty if ty.is_integer_type() => {
                    let current = match &item.value {
                        Some(ConfigValue::Int(i)) => *i,
                        _ => 0,
                    };
                    self.dialog_type = Some(DialogType::EditInt {
                        symbol: item.id.clone(),
                        current_value: current,
                        prompt: item.label.clone(),
                    });
                    self.input_buffer = current.to_string();
                    self.input_cursor = self.input_buffer.len();
                    self.focus = PanelFocus::Dialog;
                    return Ok(());
                }
                SymbolType::Hex => {
                    let current = match &item.value {
                        Some(ConfigValue::Hex(h)) => h.clone(),
                        _ => "0x0".to_string(),
                    };
                    self.dialog_type = Some(DialogType::EditHex {
                        symbol: item.id.clone(),
                        current_value: current.clone(),
                        prompt: item.label.clone(),
                    });
                    self.input_buffer = current;
                    self.input_cursor = self.input_buffer.len();
                    self.focus = PanelFocus::Dialog;
                    return Ok(());
                }
                SymbolType::Range(_) => {
                    let current = match &item.value {
                        Some(ConfigValue::Range(r)) => r.clone(),
                        _ => "[]".to_string(),
                    };
                    self.dialog_type = Some(DialogType::EditRange {
                        symbol: item.id.clone(),
                        current_value: current.clone(),
                        prompt: item.label.clone(),
                    });
                    self.input_buffer = current;
                    self.input_cursor = self.input_buffer.len();
                    self.focus = PanelFocus::Dialog;
                    return Ok(());
                }
                _ => {
                    // Fall through to toggle logic for Bool/Tristate
                }
            }
        }

        // Toggle value (for Bool/Tristate)
        let new_value = match &item.value {
            Some(ConfigValue::Bool(b)) => Some(ConfigValue::Bool(!b)),
            Some(ConfigValue::Tristate(t)) => Some(ConfigValue::Tristate(match t {
                TristateValue::No => TristateValue::Yes,
                TristateValue::Yes => TristateValue::Module,
                TristateValue::Module => TristateValue::No,
            })),
            _ => None,
        };

        if let Some(new_val) = new_value {
            let is_enabling = matches!(
                new_val,
                ConfigValue::Bool(true)
                    | ConfigValue::Tristate(TristateValue::Yes | TristateValue::Module)
            );

            if is_enabling {
                // Check dependencies before enabling
                match self.engine.can_enable(&item_id) {
                    Ok(_) => {
                        // Apply the change
                        self.apply_value_change(&item_id, new_val.clone())?;

                        // Apply select cascade
                        let selected = self.engine.apply_selects(&item_id);
                        if !selected.is_empty() {
                            self.status_message = Some(format!(
                                " {} enabled (also enabled: {})",
                                item_id,
                                selected.join(", ")
                            ));
                        } else {
                            self.status_message = Some(format!(" {} enabled", item_id));
                        }

                        // Check for implied symbols
                        let implied = self.engine.get_implied_symbols(&item_id);
                        if !implied.is_empty() {
                            // Show suggestion dialog
                            self.dialog_type = Some(DialogType::ImplySuggestion { implied });
                        }
                    }
                    Err(e) => {
                        // Show error dialog
                        self.dialog_type = Some(DialogType::DependencyError(e));
                        return Ok(());
                    }
                }
            } else {
                // Disabling
                match self.engine.can_disable(&item_id) {
                    Ok(_) => {
                        // Check what will be affected
                        let affected = self.engine.check_disable_cascade(&item_id);

                        if !affected.is_empty() {
                            // Warn user
                            self.dialog_type = Some(DialogType::CascadeWarning {
                                symbol: item_id.clone(),
                                affected,
                            });
                        } else {
                            self.apply_value_change(&item_id, new_val)?;
                            self.status_message = Some(format!(" {} disabled", item_id));
                        }
                    }
                    Err(e) => {
                        self.dialog_type = Some(DialogType::DependencyError(e));
                        return Ok(());
                    }
                }
            }

            // Force UI refresh
            self.sync_ui_state_from_symbol_table()?;
            self.update_enabled_states()?;
        }

        Ok(())
    }

    /// Get all option IDs belonging to a choice.
    ///
    /// Returns a vector of config option IDs that are children of the specified choice.
    /// Used by `handle_choice_selection` to implement mutual exclusion.
    fn get_choice_options(&self, choice_id: &str) -> Vec<String> {
        self.config_state
            .all_items
            .iter()
            .filter(|item| {
                item.parent_choice
                    .as_ref()
                    .map(|pc| pc == choice_id)
                    .unwrap_or(false)
            })
            .map(|item| item.id.clone())
            .collect()
    }

    /// Handle choice selection with mutual exclusion.
    ///
    /// This method enforces Kconfig's choice mutual exclusion semantics:
    /// when a user selects one option in a choice, all other options are automatically deselected.
    ///
    /// # Arguments
    /// * `choice_id` - The ID of the parent choice
    /// * `selected_option` - The ID of the option being selected
    ///
    /// # Behavior
    /// 1. Gets all options belonging to the choice
    /// 2. Disables all options except the selected one (mutual exclusion)
    /// 3. Enables the selected option
    /// 4. Updates UI state to reflect changes
    fn handle_choice_selection(&mut self, choice_id: &str, selected_option: &str) -> Result<()> {
        if let Err(err) = self.engine.can_enable(selected_option) {
            self.dialog_type = Some(DialogType::DependencyError(err));
            return Ok(());
        }

        // 1. Get all options in this choice
        let choice_options = self.get_choice_options(choice_id);

        // 2. Disable all other options (mutual exclusion)
        for option_id in &choice_options {
            if option_id != selected_option {
                self.apply_value_change(option_id, ConfigValue::Bool(false))?;
            }
        }

        // 3. Enable the selected option
        self.apply_value_change(selected_option, ConfigValue::Bool(true))?;
        let selected = self.engine.apply_selects(selected_option);
        let implied = self.engine.get_implied_symbols(selected_option);
        if !implied.is_empty() {
            self.dialog_type = Some(DialogType::ImplySuggestion { implied });
        }

        // 4. Update UI state
        self.sync_ui_state_from_symbol_table()?;
        self.update_enabled_states()?;

        // 5. Show status message
        self.status_message = if selected.is_empty() {
            Some(format!(" {} selected", selected_option))
        } else {
            Some(format!(
                " {} selected (also enabled: {})",
                selected_option,
                selected.join(", ")
            ))
        };

        Ok(())
    }

    fn apply_value_change(&mut self, item_id: &str, new_val: ConfigValue) -> Result<()> {
        // Update symbol table
        let value_str = match new_val {
            ConfigValue::Bool(true) => "y".to_string(),
            ConfigValue::Bool(false) => "n".to_string(),
            ConfigValue::Tristate(TristateValue::Yes) => "y".to_string(),
            ConfigValue::Tristate(TristateValue::No) => "n".to_string(),
            ConfigValue::Tristate(TristateValue::Module) => "m".to_string(),
            ConfigValue::String(s) => format!("\"{}\"", s),
            ConfigValue::Int(i) => i.to_string(),
            ConfigValue::Hex(h) => h,
            ConfigValue::Range(r) => r,
        };

        self.engine.set_value_tracked(item_id, value_str.clone());

        // Track modification
        let original = self.config_state.original_values.get(item_id).cloned();
        if original.as_deref() != Some(value_str.as_str()) {
            self.config_state
                .modified_symbols
                .insert(item_id.to_string(), value_str);
        } else {
            self.config_state.modified_symbols.remove(item_id);
        }

        Ok(())
    }

    /// Update enabled states based on dependencies
    fn update_enabled_states(&mut self) -> Result<()> {
        for item in &mut self.config_state.all_items {
            if let MenuItemKind::Config { .. } | MenuItemKind::MenuConfig { .. } = &item.kind {
                // Check if dependencies are met
                item.is_enabled = self.engine.can_enable(&item.id).is_ok();
            }
        }

        // Also update menu_tree
        for (_key, items) in self.config_state.menu_tree.iter_mut() {
            for item in items {
                if let MenuItemKind::Config { .. } | MenuItemKind::MenuConfig { .. } = &item.kind {
                    item.is_enabled = self.engine.can_enable(&item.id).is_ok();
                }
            }
        }

        Ok(())
    }

    /// Synchronize UI state from symbol table
    /// This ensures the UI always shows current symbol values
    fn sync_ui_state_from_symbol_table(&mut self) -> Result<()> {
        // Update all_items
        for item in &mut self.config_state.all_items {
            if let MenuItemKind::Config { symbol_type } | MenuItemKind::MenuConfig { symbol_type } =
                &item.kind
                && let Some(value) = self.engine.get_value(&item.id) {
                    item.value = Some(Self::parse_value(&value, symbol_type));
                }
        }

        // Update menu_tree
        for (_key, items) in self.config_state.menu_tree.iter_mut() {
            for item in items {
                if let MenuItemKind::Config { symbol_type }
                | MenuItemKind::MenuConfig { symbol_type } = &item.kind
                    && let Some(value) = self.engine.get_value(&item.id) {
                        item.value = Some(Self::parse_value(&value, symbol_type));
                    }
            }
        }

        Ok(())
    }

    /// Audit all enabled symbols to ensure their dependencies are satisfied
    fn audit_all_dependencies(&self) -> Vec<String> {
        self.engine.audit_dependency_violations()
    }

    fn save_config(&mut self) -> Result<()> {
        use std::path::Path;

        self.engine.refresh_prompt_state();

        // Audit before saving
        let violations = self.audit_all_dependencies();
        if !violations.is_empty() {
            let message = format!(
                "Configuration has {} dependency violation{}:\n{}",
                violations.len(),
                if violations.len() == 1 { "" } else { "s" },
                violations
                    .iter()
                    .take(MAX_DISPLAYED_VIOLATIONS)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n")
            );

            // Show first violation as the primary error
            if let Some(first_violation) = violations.first() {
                let parts: Vec<&str> = first_violation.splitn(2, ": ").collect();
                let (symbol, condition_str) = if parts.len() == 2 {
                    (parts[0].to_string(), parts[1].to_string())
                } else {
                    ("CONFIGURATION".to_string(), first_violation.clone())
                };

                self.dialog_type = Some(DialogType::DependencyError(
                    DependencyError::ConditionNotMet {
                        symbol,
                        condition: condition_str,
                    },
                ));
            } else {
                self.dialog_type = Some(DialogType::DependencyError(
                    DependencyError::ConditionNotMet {
                        symbol: "CONFIGURATION".to_string(),
                        condition: message,
                    },
                ));
            }
            self.focus = PanelFocus::Dialog;
            return Ok(());
        }

        self.engine.write_config(Path::new(".config"))?;

        // Clear modified symbols after save
        self.config_state.modified_symbols.clear();

        // Update original values
        for (name, symbol) in self.engine.symbols().all_symbols() {
            if let Some(value) = &symbol.value {
                self.config_state
                    .original_values
                    .insert(name.clone(), value.clone());
            }
        }

        self.status_message = Some(" Configuration saved to .config".to_string());
        Ok(())
    }

    fn render_dependency_error_dialog(&self, frame: &mut Frame, error: &DependencyError) {
        let area = self.centered_rect(60, 40, frame.size());

        let message = match error {
            DependencyError::DependencyNotMet { symbol, required } => {
                vec![
                    Line::from("⚠️  Dependency Not Met"),
                    Line::from(""),
                    Line::from(format!("Cannot enable: {}", symbol)),
                    Line::from(""),
                    Line::from(format!("Requires: {} (currently disabled)", required)),
                    Line::from(""),
                    Line::from("Press ESC to close"),
                ]
            }
            DependencyError::SelectedBy { symbol, selector } => {
                vec![
                    Line::from("⚠️  Cannot Disable"),
                    Line::from(""),
                    Line::from(format!("Cannot disable: {}", symbol)),
                    Line::from(""),
                    Line::from(format!("Selected by: {} (currently enabled)", selector)),
                    Line::from(""),
                    Line::from("Press ESC to close"),
                ]
            }
            DependencyError::ConditionNotMet { symbol, condition } => {
                vec![
                    Line::from("⚠️  Condition Not Met"),
                    Line::from(""),
                    Line::from(format!("Cannot enable: {}", symbol)),
                    Line::from(""),
                    Line::from(format!("Condition: {}", condition)),
                    Line::from(""),
                    Line::from("Press ESC to close"),
                ]
            }
            _ => vec![Line::from(format!("Error: {}", error))],
        };

        let dialog = Paragraph::new(message).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Dependency Error ")
                .style(self.theme.get_warning_style()),
        );

        frame.render_widget(dialog, area);
    }

    fn render_cascade_warning_dialog(&self, frame: &mut Frame, symbol: &str, affected: &[String]) {
        let area = self.centered_rect(60, 50, frame.size());

        let mut lines = vec![
            Line::from("⚠️  Cascade Warning"),
            Line::from(""),
            Line::from(format!("Disabling {} will also affect:", symbol)),
            Line::from(""),
        ];

        for affected_symbol in affected {
            lines.push(Line::from(format!("  • {}", affected_symbol)));
        }

        lines.push(Line::from(""));
        lines.push(Line::from("Continue? [Y/n/ESC]"));

        let dialog = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Warning ")
                .style(self.theme.get_warning_style()),
        );

        frame.render_widget(dialog, area);
    }

    fn render_imply_suggestion_dialog(&self, frame: &mut Frame, implied: &[String]) {
        let area = self.centered_rect(60, 40, frame.size());

        let mut lines = vec![
            Line::from("💡 Suggestion"),
            Line::from(""),
            Line::from("The following options are recommended:"),
            Line::from(""),
        ];

        for symbol in implied {
            lines.push(Line::from(format!("  • {}", symbol)));
        }

        lines.push(Line::from(""));
        lines.push(Line::from("Enable them? [Y/n/ESC]"));

        let dialog = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Suggestion ")
                .style(self.theme.get_info_style()),
        );

        frame.render_widget(dialog, area);
    }

    fn render_input_dialog(
        &self,
        frame: &mut Frame,
        prompt: &str,
        symbol: &str,
        type_name: &str,
        hint: &str,
    ) {
        let dialog_width = frame.size().width.min(70);
        let dialog_height = 12;
        let x = (frame.size().width.saturating_sub(dialog_width)) / 2;
        let y = (frame.size().height.saturating_sub(dialog_height)) / 2;
        let dialog_area = Rect::new(x, y, dialog_width, dialog_height);

        // Clear background
        let bg = Block::default().style(Style::default().bg(ratatui::style::Color::Black));
        frame.render_widget(bg, frame.size());

        // Dialog box
        let title = format!(" Edit {} ({}) ", prompt, type_name);
        let dialog_block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(ratatui::style::Color::Cyan));
        frame.render_widget(dialog_block, dialog_area);

        // Content area with margin
        let inner_width = dialog_width.saturating_sub(4);
        let inner_height = dialog_height.saturating_sub(2);
        let inner = Rect::new(x + 2, y + 1, inner_width, inner_height);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2), // Symbol info
                Constraint::Length(1), // Spacer
                Constraint::Length(3), // Input box
                Constraint::Length(1), // Spacer
                Constraint::Length(2), // Hint
                Constraint::Min(0),    // Spacer
            ])
            .split(inner);

        // Symbol info
        let info = Paragraph::new(format!("Symbol: {}", symbol))
            .style(Style::default().fg(ratatui::style::Color::Gray));
        frame.render_widget(info, chunks[0]);

        // Input box with cursor - handle scrolling and UTF-8 safely
        let max_display_width = inner_width.saturating_sub(4) as usize;
        let display_start = if self.input_cursor >= max_display_width {
            self.input_cursor.saturating_sub(max_display_width - 1)
        } else {
            0
        };
        let display_end = std::cmp::min(display_start + max_display_width, self.input_buffer.len());

        // Use safe UTF-8 slicing
        let visible_text = if display_start < self.input_buffer.len() {
            &self.input_buffer[display_start..display_end]
        } else {
            ""
        };
        let cursor_pos = self.input_cursor.saturating_sub(display_start);

        // Build display string safely using character iteration
        let input_display = if cursor_pos < visible_text.len() {
            let before = visible_text.chars().take(cursor_pos).collect::<String>();
            let after = visible_text.chars().skip(cursor_pos).collect::<String>();
            format!("│ {}█{} │", before, after)
        } else {
            format!("│ {}█ │", visible_text)
        };

        let input_box = Paragraph::new(vec![
            Line::from("┌───────────────────────────────────────┐"),
            Line::from(input_display),
            Line::from("└───────────────────────────────────────┘"),
        ])
        .style(Style::default().fg(ratatui::style::Color::White));
        frame.render_widget(input_box, chunks[2]);

        // Hint
        let hint_text = Paragraph::new(vec![
            Line::from(hint).style(Style::default().fg(ratatui::style::Color::Yellow)),
            Line::from("ESC: Cancel | Enter: Save")
                .style(Style::default().fg(ratatui::style::Color::Gray)),
        ]);
        frame.render_widget(hint_text, chunks[4]);
    }

    // Validation functions
    fn validate_int(input: &str) -> Option<i64> {
        input.trim().parse::<i64>().ok()
    }

    fn validate_hex(input: &str) -> Option<String> {
        let trimmed = input.trim();
        if !trimmed.starts_with("0x") && !trimmed.starts_with("0X") {
            return None;
        }
        let hex_part = &trimmed[2..];
        if hex_part.is_empty() || !hex_part.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        Some(format!("0x{}", hex_part.to_lowercase()))
    }

    fn validate_range(input: &str) -> Option<String> {
        let trimmed = input.trim();

        // Must start with [ and end with ]
        if !trimmed.starts_with('[') || !trimmed.ends_with(']') {
            return None;
        }

        // Empty array is valid
        if trimmed == "[]" {
            return Some(trimmed.to_string());
        }

        // Use safer stripping approach
        let inner = trimmed
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))?;

        // Split by comma and check each element
        let items: Vec<&str> = inner.split(',').map(|s| s.trim()).collect();

        // Must have at least one non-empty item
        if items.iter().all(|s| s.is_empty()) {
            return None;
        }

        // Reconstruct with consistent spacing
        let normalized_items: Vec<String> = items
            .iter()
            .filter(|s| !s.is_empty())
            .map(|s| s.trim().to_string())
            .collect();

        Some(format!("[{}]", normalized_items.join(", ")))
    }

    fn save_input_dialog(&mut self) -> Result<()> {
        if let Some(dialog_type) = &self.dialog_type.clone() {
            match dialog_type {
                DialogType::EditString { symbol, .. } => {
                    let new_value = self.input_buffer.clone();
                    self.update_config_value(symbol, ConfigValue::String(new_value.clone()))?;
                    self.engine
                        .set_value_tracked(symbol, format!("\"{}\"", new_value));
                    self.status_message = Some(format!("✓ {} updated", symbol));
                }
                DialogType::EditInt { symbol, .. } => {
                    if let Some(value) = Self::validate_int(&self.input_buffer) {
                        self.update_config_value(symbol, ConfigValue::Int(value))?;
                        self.engine.set_value_tracked(symbol, value.to_string());
                        self.status_message = Some(format!("✓ {} = {}", symbol, value));
                    } else {
                        self.status_message = Some("✗ Invalid integer".to_string());
                        return Ok(()); // Don't close dialog
                    }
                }
                DialogType::EditHex { symbol, .. } => {
                    if let Some(value) = Self::validate_hex(&self.input_buffer) {
                        self.update_config_value(symbol, ConfigValue::Hex(value.clone()))?;
                        self.engine.set_value_tracked(symbol, value.clone());
                        self.status_message = Some(format!("✓ {} = {}", symbol, value));
                    } else {
                        self.status_message = Some("✗ Invalid hex (use 0xABC format)".to_string());
                        return Ok(());
                    }
                }
                DialogType::EditRange { symbol, .. } => {
                    if let Some(value) = Self::validate_range(&self.input_buffer) {
                        self.update_config_value(symbol, ConfigValue::Range(value.clone()))?;
                        self.engine.set_value_tracked(symbol, value.clone());
                        self.status_message = Some(format!("✓ {} = {}", symbol, value));
                    } else {
                        self.status_message =
                            Some("✗ Invalid range (use [item1, item2, ...] format)".to_string());
                        return Ok(());
                    }
                }
                _ => {}
            }
        }

        self.dialog_type = None;
        self.focus = PanelFocus::MenuTree;
        self.input_buffer.clear();
        Ok(())
    }

    fn update_config_value(&mut self, symbol: &str, new_value: ConfigValue) -> Result<()> {
        // Update in all_items
        for item in &mut self.config_state.all_items {
            if item.id == symbol {
                item.value = Some(new_value.clone());
                break;
            }
        }

        // Update in menu_tree
        for (_key, items) in self.config_state.menu_tree.iter_mut() {
            for item in items {
                if item.id == symbol {
                    item.value = Some(new_value.clone());
                    break;
                }
            }
        }

        // Track modification
        let value_str = match &new_value {
            ConfigValue::String(s) => format!("\"{}\"", s),
            ConfigValue::Int(i) => i.to_string(),
            ConfigValue::Hex(h) => h.clone(),
            ConfigValue::Range(r) => r.clone(),
            _ => return Ok(()),
        };

        let original = self.config_state.original_values.get(symbol).cloned();
        if original.as_deref() != Some(value_str.as_str()) {
            self.config_state
                .modified_symbols
                .insert(symbol.to_string(), value_str);
        } else {
            self.config_state.modified_symbols.remove(symbol);
        }

        Ok(())
    }

    /// Helper function to format an Expr into a human-readable string
    fn format_expr(expr: &Expr) -> String {
        match expr {
            Expr::Symbol(s) => s.clone(),
            Expr::Const(c) => c.clone(),
            Expr::ShellExpr(e) => format!("shell({})", e),
            Expr::Not(e) => format!("!{}", Self::format_expr(e)),
            Expr::And(left, right) => {
                format!(
                    "{} && {}",
                    Self::format_expr(left),
                    Self::format_expr(right)
                )
            }
            Expr::Or(left, right) => {
                format!(
                    "{} || {}",
                    Self::format_expr(left),
                    Self::format_expr(right)
                )
            }
            Expr::Equal(left, right) => {
                format!("{} = {}", Self::format_expr(left), Self::format_expr(right))
            }
            Expr::NotEqual(left, right) => {
                format!(
                    "{} != {}",
                    Self::format_expr(left),
                    Self::format_expr(right)
                )
            }
            Expr::Less(left, right) => {
                format!("{} < {}", Self::format_expr(left), Self::format_expr(right))
            }
            Expr::LessEqual(left, right) => {
                format!(
                    "{} <= {}",
                    Self::format_expr(left),
                    Self::format_expr(right)
                )
            }
            Expr::Greater(left, right) => {
                format!("{} > {}", Self::format_expr(left), Self::format_expr(right))
            }
            Expr::GreaterEqual(left, right) => {
                format!(
                    "{} >= {}",
                    Self::format_expr(left),
                    Self::format_expr(right)
                )
            }
        }
    }

    /// Find the menu path and index for a given item ID
    /// Returns (path, index) where path is the parent menu path and index is the position in that menu
    fn find_item_location(&self, item_id: &str) -> Option<(Vec<String>, usize)> {
        // Check root level first
        if let Some(root_items) = self.config_state.menu_tree.get("root") {
            for (idx, item) in root_items.iter().enumerate() {
                if item.id == item_id {
                    return Some((Vec::new(), idx));
                }
            }
        }

        // Check all other menu levels
        for (parent_key, items) in &self.config_state.menu_tree {
            if parent_key == "root" {
                continue;
            }

            for (idx, item) in items.iter().enumerate() {
                if item.id == item_id {
                    // Build the path to this item
                    let path = self.build_path_to_menu(parent_key);
                    return Some((path, idx));
                }
            }
        }

        None
    }

    /// Build the path to a specific menu by its ID
    ///
    /// # Limitation
    /// This is a simplified implementation that handles one level of nesting.
    /// For deeply nested menus (menu within menu within menu), only the immediate
    /// parent menu will be in the path. This is sufficient for most Kconfig files
    /// which typically have a flat or shallow menu structure (e.g., root -> menu -> items).
    ///
    /// If full path resolution is needed in the future, this would require building
    /// a parent map during ConfigState construction or performing a recursive search.
    fn build_path_to_menu(&self, menu_id: &str) -> Vec<String> {
        // For simple case, we just need the menu_id itself
        // In a more complex tree, we'd need to recursively build the path
        // For now, check if this is a direct child of root
        if let Some(root_items) = self.config_state.menu_tree.get("root") {
            for item in root_items {
                if item.id == menu_id && item.has_children {
                    return vec![menu_id.to_string()];
                }
            }
        }

        // Otherwise, we need to search through all menus to build the full path
        // This is a simplified implementation that handles one level of nesting
        vec![menu_id.to_string()]
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{Mutex, OnceLock},
    };

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tempfile::TempDir;

    use super::*;
    use crate::kconfig::{Parser, SymbolTable};

    fn cwd_test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn test_validate_int() {
        assert_eq!(MenuConfigApp::validate_int("123"), Some(123));
        assert_eq!(MenuConfigApp::validate_int("-456"), Some(-456));
        assert_eq!(MenuConfigApp::validate_int("0"), Some(0));
        assert_eq!(MenuConfigApp::validate_int("  789  "), Some(789));
        assert_eq!(MenuConfigApp::validate_int("abc"), None);
        assert_eq!(MenuConfigApp::validate_int(""), None);
        assert_eq!(MenuConfigApp::validate_int("12.34"), None);
    }

    #[test]
    fn test_validate_hex() {
        assert_eq!(
            MenuConfigApp::validate_hex("0xFF"),
            Some("0xff".to_string())
        );
        assert_eq!(
            MenuConfigApp::validate_hex("0x1A2B"),
            Some("0x1a2b".to_string())
        );
        assert_eq!(
            MenuConfigApp::validate_hex("0X100"),
            Some("0x100".to_string())
        );
        assert_eq!(
            MenuConfigApp::validate_hex("0xaBcDeF"),
            Some("0xabcdef".to_string())
        );
        assert_eq!(MenuConfigApp::validate_hex("0x0"), Some("0x0".to_string()));
        assert_eq!(
            MenuConfigApp::validate_hex("  0xFF  "),
            Some("0xff".to_string())
        );
        assert_eq!(MenuConfigApp::validate_hex("FF"), None);
        assert_eq!(MenuConfigApp::validate_hex("0x"), None);
        assert_eq!(MenuConfigApp::validate_hex("0xGG"), None);
        assert_eq!(MenuConfigApp::validate_hex(""), None);
    }

    #[test]
    fn test_parse_value_hex() {
        // Test hex values with 0x prefix (should normalize to lowercase)
        assert_eq!(
            MenuConfigApp::parse_value("0x40000000", &SymbolType::Hex),
            ConfigValue::Hex("0x40000000".to_string())
        );
        assert_eq!(
            MenuConfigApp::parse_value("0xFF", &SymbolType::Hex),
            ConfigValue::Hex("0xff".to_string())
        );
        assert_eq!(
            MenuConfigApp::parse_value("0X100", &SymbolType::Hex),
            ConfigValue::Hex("0x100".to_string())
        );

        // Test decimal values (should be converted to hex format)
        assert_eq!(
            MenuConfigApp::parse_value("1073741824", &SymbolType::Hex),
            ConfigValue::Hex("0x40000000".to_string())
        );
        assert_eq!(
            MenuConfigApp::parse_value("255", &SymbolType::Hex),
            ConfigValue::Hex("0xff".to_string())
        );
        assert_eq!(
            MenuConfigApp::parse_value("0", &SymbolType::Hex),
            ConfigValue::Hex("0x0".to_string())
        );

        // Test negative values
        assert_eq!(
            MenuConfigApp::parse_value("-255", &SymbolType::Hex),
            ConfigValue::Hex("-0xff".to_string())
        );

        // Test with whitespace
        assert_eq!(
            MenuConfigApp::parse_value("  0xFF  ", &SymbolType::Hex),
            ConfigValue::Hex("0xff".to_string())
        );
        assert_eq!(
            MenuConfigApp::parse_value("  255  ", &SymbolType::Hex),
            ConfigValue::Hex("0xff".to_string())
        );

        // Test edge cases
        // Empty hex prefix (0x with no digits) - should be kept as-is
        assert_eq!(
            MenuConfigApp::parse_value("0x", &SymbolType::Hex),
            ConfigValue::Hex("0x".to_string())
        );
        // i64::MIN overflow case
        assert_eq!(
            MenuConfigApp::parse_value("-9223372036854775808", &SymbolType::Hex),
            ConfigValue::Hex("-0x8000000000000000".to_string())
        );

        // Test invalid values (should keep as-is)
        assert_eq!(
            MenuConfigApp::parse_value("invalid", &SymbolType::Hex),
            ConfigValue::Hex("invalid".to_string())
        );
    }

    #[test]
    fn test_cascade_disable_clears_dependent_symbols() {
        let temp_dir = TempDir::new().unwrap();
        let kconfig_path = temp_dir.path().join("Kconfig");

        let kconfig_content = r#"
config KFEAT_FS
    bool "Enable filesystem support"
    default y

config KFEAT_FS_EXT4
    bool "ext4"
    depends on KFEAT_FS

config KFEAT_FS_TIMES
    bool "Enable filesystem timestamps"
    depends on KFEAT_FS_EXT4
"#;

        fs::write(&kconfig_path, kconfig_content).unwrap();

        let mut parser = Parser::new(&kconfig_path, temp_dir.path()).unwrap();
        let ast = parser.parse().unwrap();

        let mut symbol_table = SymbolTable::new();
        symbol_table.add_symbol("KFEAT_FS".to_string(), SymbolType::Bool);
        symbol_table.add_symbol("KFEAT_FS_EXT4".to_string(), SymbolType::Bool);
        symbol_table.add_symbol("KFEAT_FS_TIMES".to_string(), SymbolType::Bool);
        symbol_table.set_value("KFEAT_FS", "y".to_string());
        symbol_table.set_value("KFEAT_FS_EXT4", "y".to_string());
        symbol_table.set_value("KFEAT_FS_TIMES", "y".to_string());

        let mut app = MenuConfigApp::new(ast.entries, symbol_table).unwrap();
        let affected = app.engine().check_disable_cascade("KFEAT_FS");
        assert_eq!(
            affected,
            vec!["KFEAT_FS_EXT4".to_string(), "KFEAT_FS_TIMES".to_string()]
        );

        app.dialog_type = Some(DialogType::CascadeWarning {
            symbol: "KFEAT_FS".to_string(),
            affected,
        });
        let key = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
        app.handle_cascade_warning_dialog_key(key).unwrap();

        assert_eq!(app.engine().get_value("KFEAT_FS").as_deref(), Some("n"));
        assert_eq!(
            app.engine().get_value("KFEAT_FS_EXT4").as_deref(),
            Some("n")
        );
        assert_eq!(
            app.engine().get_value("KFEAT_FS_TIMES").as_deref(),
            Some("n")
        );
    }

    #[test]
    fn test_choice_depends_propagates_to_options_for_dependency_checks() {
        let temp_dir = TempDir::new().unwrap();
        let kconfig_path = temp_dir.path().join("Kconfig");

        let kconfig_content = r#"
config KFEAT_FS
    bool "Enable filesystem support"
    default n

choice
    prompt "Default filesystem"
    depends on KFEAT_FS
    default KFEAT_FS_EXT4

config KFEAT_FS_EXT4
    bool "ext4"

config KFEAT_FS_FAT
    bool "fat"

endchoice
"#;

        fs::write(&kconfig_path, kconfig_content).unwrap();

        let mut parser = Parser::new(&kconfig_path, temp_dir.path()).unwrap();
        let ast = parser.parse().unwrap();

        let mut symbol_table = SymbolTable::new();
        symbol_table.add_symbol("KFEAT_FS".to_string(), SymbolType::Bool);
        symbol_table.add_symbol("KFEAT_FS_EXT4".to_string(), SymbolType::Bool);
        symbol_table.add_symbol("KFEAT_FS_FAT".to_string(), SymbolType::Bool);
        symbol_table.set_value("KFEAT_FS", "n".to_string());
        symbol_table.set_value("KFEAT_FS_EXT4", "y".to_string());

        let app = MenuConfigApp::new(ast.entries, symbol_table).unwrap();

        assert!(app.engine().can_enable("KFEAT_FS_EXT4").is_err());
    }

    #[test]
    fn test_choice_selection_applies_selects() {
        let temp_dir = TempDir::new().unwrap();
        let kconfig_path = temp_dir.path().join("Kconfig");

        let kconfig_content = r#"
choice
    prompt "Console backend"
    default CONSOLE_PL011

config CONSOLE_PL011
    bool "pl011"
    select HELPER

config CONSOLE_NS16550
    bool "ns16550"

endchoice

config HELPER
    bool "helper"
"#;

        fs::write(&kconfig_path, kconfig_content).unwrap();

        let mut parser = Parser::new(&kconfig_path, temp_dir.path()).unwrap();
        let ast = parser.parse().unwrap();

        let mut symbol_table = SymbolTable::new();
        symbol_table.add_symbol("CONSOLE_PL011".to_string(), SymbolType::Bool);
        symbol_table.add_symbol("CONSOLE_NS16550".to_string(), SymbolType::Bool);
        symbol_table.add_symbol("HELPER".to_string(), SymbolType::Bool);
        symbol_table.set_value("CONSOLE_NS16550", "y".to_string());

        let mut app = MenuConfigApp::new(ast.entries, symbol_table).unwrap();
        app.handle_choice_selection("choice_CONSOLE_PL011", "CONSOLE_PL011")
            .unwrap();

        assert_eq!(app.engine().get_value("CONSOLE_PL011").as_deref(), Some("y"));
        assert_eq!(app.engine().get_value("CONSOLE_NS16550").as_deref(), Some("n"));
        assert_eq!(app.engine().get_value("HELPER").as_deref(), Some("y"));
    }

    #[test]
    fn test_save_config_refreshes_effective_state_before_write() {
        let _guard = cwd_test_lock().lock().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        let kconfig_path = temp_dir.path().join("Kconfig");

        let kconfig_content = r#"
config LIMIT_MIN
    u32
    default 4

config LIMIT_MAX
    u32
    default 8

config COUNT
    u32 "Count"
    range LIMIT_MIN LIMIT_MAX
    default 6
"#;

        fs::write(&kconfig_path, kconfig_content).unwrap();

        let mut parser = Parser::new(&kconfig_path, temp_dir.path()).unwrap();
        let ast = parser.parse().unwrap();

        let mut symbol_table = SymbolTable::new();
        symbol_table.add_symbol("LIMIT_MIN".to_string(), SymbolType::U32);
        symbol_table.add_symbol("LIMIT_MAX".to_string(), SymbolType::U32);
        symbol_table.add_symbol("COUNT".to_string(), SymbolType::U32);
        symbol_table.set_value("LIMIT_MIN", "4".to_string());
        symbol_table.set_value("LIMIT_MAX", "8".to_string());
        symbol_table.set_value("COUNT", "99".to_string());

        let mut app = MenuConfigApp::new(ast.entries, symbol_table).unwrap();

        std::env::set_current_dir(temp_dir.path()).unwrap();
        let result = app.save_config();
        std::env::set_current_dir(old_cwd).unwrap();
        result.unwrap();

        let config = fs::read_to_string(temp_dir.path().join(".config")).unwrap();
        assert!(config.contains("COUNT=8"));
    }

    #[test]
    fn test_save_config_allows_linux_style_selected_symbol_with_unmet_direct_deps() {
        let _guard = cwd_test_lock().lock().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        let kconfig_path = temp_dir.path().join("Kconfig");

        let kconfig_content = r#"
config DEP
    bool "dep"
    default n

config HELPER
    bool "helper"
    depends on DEP

config SELECTOR
    bool "selector"
    select HELPER
"#;

        fs::write(&kconfig_path, kconfig_content).unwrap();

        let mut parser = Parser::new(&kconfig_path, temp_dir.path()).unwrap();
        let ast = parser.parse().unwrap();

        let mut symbol_table = SymbolTable::new();
        symbol_table.add_symbol("DEP".to_string(), SymbolType::Bool);
        symbol_table.add_symbol("HELPER".to_string(), SymbolType::Bool);
        symbol_table.add_symbol("SELECTOR".to_string(), SymbolType::Bool);
        symbol_table.set_value("DEP", "n".to_string());
        symbol_table.set_value("SELECTOR", "y".to_string());
        symbol_table.set_value("HELPER", "y".to_string());

        let mut app = MenuConfigApp::new(ast.entries, symbol_table).unwrap();

        std::env::set_current_dir(temp_dir.path()).unwrap();
        let result = app.save_config();
        std::env::set_current_dir(old_cwd).unwrap();
        result.unwrap();

        let config = fs::read_to_string(temp_dir.path().join(".config")).unwrap();
        assert!(config.contains("SELECTOR=y"));
        assert!(config.contains("HELPER=y"));
    }
}
