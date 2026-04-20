// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub enum SymbolType {
    Bool,
    Tristate,
    String,
    U8,
    U16,
    U32,
    U64,
    U128,
    Usize,
    I8,
    I16,
    I32,
    I64,
    I128,
    Isize,
    Hex,
    Range(RangeType),
}

/// Represents the element type of a rangetype config.
#[derive(Debug, Clone, PartialEq)]
pub enum RangeType {
    /// Array of string slices: `["&str"]` or `["String"]`
    StringArray,
    /// Array of tuples: `[(u64, u64)]` or `[(u32, u32, u32)]`
    Tuple(Vec<RustType>),
    /// Array of a single primitive type: `[u32]`, `[usize]`, etc.
    Primitive(RustType),
    /// Type not yet determined (placeholder before the default is parsed).
    Unknown,
}

impl Default for RangeType {
    fn default() -> Self {
        Self::Unknown
    }
}

/// Represents a Rust primitive or string type used inside rangetype annotations.
#[derive(Debug, Clone, PartialEq)]
pub enum RustType {
    U8,
    U16,
    U32,
    U64,
    U128,
    Usize,
    I8,
    I16,
    I32,
    I64,
    I128,
    Isize,
    Str,
    String,
}

impl SymbolType {
    /// Returns `true` if this is one of the explicit Rust integer types
    /// (`u8`…`usize`, `i8`…`isize`).
    pub fn is_integer_type(&self) -> bool {
        matches!(
            self,
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
                | SymbolType::Isize
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Symbol(String),
    Const(String),
    ShellExpr(String),
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Equal(Box<Expr>, Box<Expr>),
    NotEqual(Box<Expr>, Box<Expr>),
    Less(Box<Expr>, Box<Expr>),
    LessEqual(Box<Expr>, Box<Expr>),
    Greater(Box<Expr>, Box<Expr>),
    GreaterEqual(Box<Expr>, Box<Expr>),
}

/// Represents a default value with an optional condition
#[derive(Debug, Clone)]
pub struct DefaultValue {
    pub value: Expr,
    pub condition: Option<Expr>,
}

#[derive(Debug, Clone)]
pub struct Property {
    pub prompt: Option<String>,
    pub defaults: Vec<DefaultValue>,
    pub depends: Option<Expr>,
    pub select: Vec<(String, Option<Expr>)>,
    pub imply: Vec<(String, Option<Expr>)>,
    pub range: Option<(Expr, Expr, Option<Expr>)>,
    pub help: Option<String>,
}

impl Default for Property {
    fn default() -> Self {
        Self {
            prompt: None,
            defaults: Vec::new(),
            depends: None,
            select: Vec::new(),
            imply: Vec::new(),
            range: None,
            help: None,
        }
    }
}

impl Property {
    /// Evaluate conditional defaults in order and return the first matching value
    pub fn evaluate_default(&self, symbol_table: &crate::kconfig::SymbolTable) -> Option<String> {
        use crate::kconfig::{expr::evaluate_expr, shell_expr::evaluate_shell_expr};

        for default in &self.defaults {
            // Check condition (if any)
            if let Some(ref condition) = default.condition {
                // Skip this default if condition is not met
                if !matches!(evaluate_expr(condition, symbol_table), Ok(true)) {
                    continue;
                }
            }

            // Evaluate the value expression
            match &default.value {
                Expr::Const(val) => return Some(val.clone()),
                Expr::Symbol(sym) => {
                    if matches!(sym.as_str(), "y" | "m" | "n") {
                        return Some(sym.clone());
                    }
                    if let Some(value) = symbol_table.get_value(sym) {
                        return Some(value);
                    }
                }
                Expr::ShellExpr(shell) => {
                    if let Ok(value) = evaluate_shell_expr(shell, symbol_table) {
                        if !value.is_empty() {
                            return Some(value);
                        }
                    }
                }
                _ => {}
            }
        }
        None
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub name: String,
    pub symbol_type: SymbolType,
    pub properties: Property,
}

impl Config {
    /// Returns true if this is a derived symbol (no prompt, value always computed from defaults).
    pub fn is_derived(&self) -> bool {
        self.properties.prompt.is_none()
    }
}

#[derive(Debug, Clone)]
pub struct MenuConfig {
    pub name: String,
    pub symbol_type: SymbolType,
    pub properties: Property,
}

impl MenuConfig {
    /// Returns true if this is a derived symbol (no prompt, value always computed from defaults).
    pub fn is_derived(&self) -> bool {
        self.properties.prompt.is_none()
    }
}

#[derive(Debug, Clone)]
pub struct Choice {
    pub name: Option<String>,
    pub prompt: Option<String>,
    pub symbol_type: SymbolType,
    pub default: Option<String>,
    pub depends: Option<Expr>,
    pub options: Vec<Config>,
}

#[derive(Debug, Clone)]
pub struct Menu {
    pub title: String,
    pub depends: Option<Expr>,
    pub visible: Option<Expr>,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone)]
pub struct If {
    pub condition: Expr,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone)]
pub struct Source {
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Comment {
    pub text: String,
    pub depends: Option<Expr>,
}

#[derive(Debug, Clone)]
pub enum Entry {
    Config(Config),
    MenuConfig(MenuConfig),
    Choice(Choice),
    Menu(Menu),
    If(If),
    Source(Source),
    Comment(Comment),
    MainMenu(String),
}

#[derive(Debug, Clone)]
pub struct KconfigFile {
    pub path: PathBuf,
    pub entries: Vec<Entry>,
}
