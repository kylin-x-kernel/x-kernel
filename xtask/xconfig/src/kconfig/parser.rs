// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use crate::{
    error::{KconfigError, Result},
    kconfig::{
        ast::*,
        lexer::{Lexer, Token},
    },
};

/// Map an integer-type token to the corresponding [`SymbolType`] variant.
///
/// Panics if `token` is not one of the integer type tokens.
fn token_to_integer_symbol_type(token: &Token) -> SymbolType {
    match token {
        Token::U8 => SymbolType::U8,
        Token::U16 => SymbolType::U16,
        Token::U32 => SymbolType::U32,
        Token::U64 => SymbolType::U64,
        Token::U128 => SymbolType::U128,
        Token::Usize => SymbolType::Usize,
        Token::I8 => SymbolType::I8,
        Token::I16 => SymbolType::I16,
        Token::I32 => SymbolType::I32,
        Token::I64 => SymbolType::I64,
        Token::I128 => SymbolType::I128,
        Token::Isize => SymbolType::Isize,
        _ => unreachable!(
            "token_to_integer_symbol_type called with non-integer token: {:?}",
            token
        ),
    }
}

pub struct Parser {
    current_file: PathBuf,
    srctree: PathBuf,
    file_stack: Vec<FileContext>,
    parsed_files: HashSet<PathBuf>,
    inclusion_chain: Vec<PathBuf>,
}

#[allow(dead_code)]
struct FileContext {
    file_path: PathBuf,
    lexer: Lexer,
    current_token: Token,
}

impl Parser {
    pub fn new(kconfig_path: impl AsRef<Path>, srctree: impl AsRef<Path>) -> Result<Self> {
        let kconfig_path = kconfig_path.as_ref().to_path_buf();
        let srctree = srctree.as_ref().to_path_buf();

        if !kconfig_path.exists() {
            return Err(KconfigError::FileNotFound(kconfig_path));
        }

        let content = fs::read_to_string(&kconfig_path)?;
        let mut lexer = Lexer::new(content, kconfig_path.clone());
        let current_token = lexer.next_token()?;

        let mut parsed_files = HashSet::new();
        parsed_files.insert(kconfig_path.clone());

        Ok(Self {
            current_file: kconfig_path.clone(),
            srctree,
            file_stack: vec![FileContext {
                file_path: kconfig_path.clone(),
                lexer,
                current_token,
            }],
            parsed_files,
            inclusion_chain: vec![kconfig_path],
        })
    }

    fn current_context(&self) -> &FileContext {
        self.file_stack.last().expect("File stack is empty")
    }

    fn current_context_mut(&mut self) -> &mut FileContext {
        self.file_stack.last_mut().expect("File stack is empty")
    }

    fn advance(&mut self) -> Result<()> {
        let ctx = self.current_context_mut();
        ctx.current_token = ctx.lexer.next_token()?;
        Ok(())
    }

    fn expect(&mut self, expected: Token) -> Result<()> {
        let current = self.current_context().current_token.clone();
        if std::mem::discriminant(&current) != std::mem::discriminant(&expected) {
            return Err(KconfigError::Syntax {
                file: self.current_file.clone(),
                line: self.current_context().lexer.current_line(),
                message: format!("Expected {:?}, got {:?}", expected, current),
            });
        }
        self.advance()
    }

    fn skip_newlines(&mut self) -> Result<()> {
        while matches!(self.current_context().current_token, Token::Newline) {
            self.advance()?;
        }
        Ok(())
    }

    // Handle source directive with recursion detection
    fn handle_source(&mut self, path_expr: String) -> Result<Vec<Entry>> {
        // Resolve the path relative to srctree
        let source_path = self.srctree.join(&path_expr);

        // Check if file exists
        if !source_path.exists() {
            return Err(KconfigError::FileNotFound(source_path));
        }

        // Containment (CWE-22): a `source` directive must not escape the
        // source tree. An absolute `path_expr` or one containing `..` could
        // otherwise pull in arbitrary files. Compare canonicalized paths so
        // symlinks are resolved consistently.
        let srctree_canon = fs::canonicalize(&self.srctree)?;
        let source_canon = fs::canonicalize(&source_path)?;
        if !source_canon.starts_with(&srctree_canon) {
            return Err(KconfigError::Config(format!(
                "source directive escapes srctree: {path_expr:?} resolves to {}",
                source_canon.display()
            )));
        }

        // Check for circular dependency
        if self.inclusion_chain.contains(&source_path) {
            let chain = self
                .inclusion_chain
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(" -> ");
            return Err(KconfigError::RecursiveSource {
                chain: format!("{} -> {}", chain, source_path.display()),
            });
        }

        // Check if file was already parsed (but not in current chain - that's circular)
        if self.parsed_files.contains(&source_path) {
            // Already parsed, skip
            return Ok(vec![]);
        }

        // Mark as parsed
        self.parsed_files.insert(source_path.clone());
        self.inclusion_chain.push(source_path.clone());

        // Read the source file
        let content = fs::read_to_string(&source_path)?;
        let mut lexer = Lexer::new(content, source_path.clone());
        let current_token = lexer.next_token()?;

        // Push new file context
        let old_file = self.current_file.clone();
        self.current_file = source_path.clone();
        self.file_stack.push(FileContext {
            file_path: source_path.clone(),
            lexer,
            current_token,
        });

        // Parse the source file
        let entries = self.parse_entries()?;

        // Pop file context
        self.file_stack.pop();
        self.current_file = old_file;
        self.inclusion_chain.pop();

        Ok(entries)
    }

    pub fn parse(&mut self) -> Result<KconfigFile> {
        let entries = self.parse_entries()?;
        Ok(KconfigFile {
            path: self.inclusion_chain[0].clone(),
            entries,
        })
    }

    fn parse_entries(&mut self) -> Result<Vec<Entry>> {
        let mut entries = Vec::new();

        self.skip_newlines()?;

        while !matches!(self.current_context().current_token, Token::Eof) {
            match &self.current_context().current_token.clone() {
                Token::Config => {
                    entries.push(Entry::Config(self.parse_config()?));
                }
                Token::MenuConfig => {
                    entries.push(Entry::MenuConfig(self.parse_menuconfig()?));
                }
                Token::Choice => {
                    entries.push(Entry::Choice(self.parse_choice()?));
                }
                Token::Menu => {
                    entries.push(Entry::Menu(self.parse_menu()?));
                }
                Token::If => {
                    entries.push(Entry::If(self.parse_if()?));
                }
                Token::Source => {
                    self.advance()?; // consume 'source'
                    let path = self.parse_string()?;
                    self.skip_newlines()?;

                    // Recursively parse the source file
                    let source_entries = self.handle_source(path.clone())?;
                    entries.extend(source_entries);

                    // Also add the source entry itself
                    entries.push(Entry::Source(Source {
                        path: PathBuf::from(path),
                    }));
                }
                Token::Comment => {
                    entries.push(Entry::Comment(self.parse_comment()?));
                }
                Token::MainMenu => {
                    self.advance()?; // consume 'mainmenu'
                    let title = self.parse_string()?;
                    self.skip_newlines()?;
                    entries.push(Entry::MainMenu(title));
                }
                Token::EndMenu | Token::EndIf | Token::EndChoice => {
                    // End of block
                    break;
                }
                Token::Newline => {
                    self.advance()?;
                }
                Token::Eof => break,
                _ => {
                    return Err(KconfigError::Syntax {
                        file: self.current_file.clone(),
                        line: self.current_context().lexer.current_line(),
                        message: format!(
                            "Unexpected token: {:?}",
                            self.current_context().current_token
                        ),
                    });
                }
            }
        }

        Ok(entries)
    }

    fn parse_config(&mut self) -> Result<Config> {
        self.advance()?; // consume 'config'

        let name = match &self.current_context().current_token {
            Token::Identifier(s) => s.clone(),
            _ => {
                return Err(KconfigError::Syntax {
                    file: self.current_file.clone(),
                    line: self.current_context().lexer.current_line(),
                    message: "Expected identifier after 'config'".to_string(),
                });
            }
        };
        self.advance()?;
        self.skip_newlines()?;

        let (symbol_type, properties) = self.parse_config_options()?;

        Ok(Config {
            name,
            symbol_type,
            properties,
        })
    }

    fn parse_menuconfig(&mut self) -> Result<MenuConfig> {
        self.advance()?; // consume 'menuconfig'

        let name = match &self.current_context().current_token {
            Token::Identifier(s) => s.clone(),
            _ => {
                return Err(KconfigError::Syntax {
                    file: self.current_file.clone(),
                    line: self.current_context().lexer.current_line(),
                    message: "Expected identifier after 'menuconfig'".to_string(),
                });
            }
        };
        self.advance()?;
        self.skip_newlines()?;

        let (symbol_type, properties) = self.parse_config_options()?;

        Ok(MenuConfig {
            name,
            symbol_type,
            properties,
        })
    }

    fn parse_config_options(&mut self) -> Result<(SymbolType, Property)> {
        let mut symbol_type = SymbolType::Bool;
        let mut properties = Property::default();

        while !matches!(
            self.current_context().current_token,
            Token::Config
                | Token::MenuConfig
                | Token::Choice
                | Token::Menu
                | Token::EndMenu
                | Token::If
                | Token::EndIf
                | Token::Source
                | Token::Comment
                | Token::EndChoice
                | Token::Eof
        ) {
            match &self.current_context().current_token.clone() {
                Token::Bool => {
                    self.advance()?;
                    symbol_type = SymbolType::Bool;
                    if let Ok(prompt) = self.try_parse_prompt() {
                        properties.prompt = Some(prompt);
                    }
                }
                Token::Tristate => {
                    self.advance()?;
                    symbol_type = SymbolType::Tristate;
                    if let Ok(prompt) = self.try_parse_prompt() {
                        properties.prompt = Some(prompt);
                    }
                }
                Token::String => {
                    self.advance()?;
                    symbol_type = SymbolType::String;
                    if let Ok(prompt) = self.try_parse_prompt() {
                        properties.prompt = Some(prompt);
                    }
                }
                Token::U8
                | Token::U16
                | Token::U32
                | Token::U64
                | Token::U128
                | Token::Usize
                | Token::I8
                | Token::I16
                | Token::I32
                | Token::I64
                | Token::I128
                | Token::Isize => {
                    let tok = self.current_context().current_token.clone();
                    self.advance()?;
                    symbol_type = token_to_integer_symbol_type(&tok);
                    if let Ok(prompt) = self.try_parse_prompt() {
                        properties.prompt = Some(prompt);
                    }
                }
                Token::Hex => {
                    self.advance()?;
                    symbol_type = SymbolType::Hex;
                    if let Ok(prompt) = self.try_parse_prompt() {
                        properties.prompt = Some(prompt);
                    }
                }
                Token::RangeType => {
                    self.advance()?;
                    symbol_type = SymbolType::Range(RangeType::Unknown);
                    if let Ok(prompt) = self.try_parse_prompt() {
                        properties.prompt = Some(prompt);
                    }
                }
                Token::Prompt => {
                    self.advance()?;
                    properties.prompt = Some(self.parse_string()?);
                    if matches!(self.current_context().current_token, Token::If) {
                        self.advance()?;
                        // Parse if condition (simplified)
                    }
                }
                Token::Default => {
                    self.advance()?;

                    // For rangetype configs, the default is a type annotation, not a value
                    if matches!(symbol_type, SymbolType::Range(_))
                        && matches!(self.current_context().current_token, Token::LBracket)
                    {
                        let range_type = self.parse_range_type_annotation()?;
                        symbol_type = SymbolType::Range(range_type);
                    } else {
                        // Check if it's an array literal
                        let value =
                            if matches!(self.current_context().current_token, Token::LBracket) {
                                self.parse_array_literal()?
                            } else {
                                self.parse_expr()?
                            };

                        // Check for optional 'if' condition
                        let condition = if matches!(self.current_context().current_token, Token::If)
                        {
                            self.advance()?; // consume 'if'
                            Some(self.parse_expr()?)
                        } else {
                            None
                        };

                        properties.defaults.push(DefaultValue { value, condition });
                    }
                }
                Token::Depends => {
                    self.advance()?;
                    self.expect(Token::On)?;
                    properties.depends = Some(self.parse_expr()?);
                }
                Token::Select => {
                    self.advance()?;
                    let sym = self.parse_identifier()?;
                    let cond = if matches!(self.current_context().current_token, Token::If) {
                        self.advance()?;
                        Some(self.parse_expr()?)
                    } else {
                        None
                    };
                    properties.select.push((sym, cond));
                }
                Token::Imply => {
                    self.advance()?;
                    let sym = self.parse_identifier()?;
                    let cond = if matches!(self.current_context().current_token, Token::If) {
                        self.advance()?;
                        Some(self.parse_expr()?)
                    } else {
                        None
                    };
                    properties.imply.push((sym, cond));
                }
                Token::Range => {
                    self.advance()?;
                    let min = self.parse_range_bound()?;
                    let max = self.parse_range_bound()?;
                    let cond = if matches!(self.current_context().current_token, Token::If) {
                        self.advance()?;
                        Some(self.parse_expr()?)
                    } else {
                        None
                    };
                    properties.range = Some((min, max, cond));
                }
                Token::Help => {
                    // Don't advance yet - skip help text directly from lexer
                    let ctx = self.current_context_mut();
                    let help_text = ctx.lexer.skip_help_text();
                    properties.help = Some(help_text);
                    // Now get the next token after skipping help
                    ctx.current_token = ctx.lexer.next_token()?;
                }
                Token::Newline => {
                    self.advance()?;
                }
                _ => break,
            }
        }

        Ok((symbol_type, properties))
    }

    fn parse_choice(&mut self) -> Result<Choice> {
        self.advance()?; // consume 'choice'
        self.skip_newlines()?;

        let name = None;
        let mut prompt = None;
        let mut symbol_type = SymbolType::Bool;
        let mut default = None;
        let mut depends = None;
        let mut options = Vec::new();

        // Parse choice options
        while !matches!(self.current_context().current_token, Token::EndChoice) {
            match &self.current_context().current_token.clone() {
                Token::Prompt => {
                    self.advance()?;
                    prompt = Some(self.parse_string()?);
                }
                Token::Bool => {
                    self.advance()?;
                    symbol_type = SymbolType::Bool;
                }
                Token::Tristate => {
                    self.advance()?;
                    symbol_type = SymbolType::Tristate;
                }
                Token::Default => {
                    self.advance()?;
                    default = Some(self.parse_identifier()?);
                }
                Token::Depends => {
                    self.advance()?;
                    self.expect(Token::On)?;
                    depends = Some(self.parse_expr()?);
                }
                Token::Config => {
                    options.push(self.parse_config()?);
                }
                Token::Newline => {
                    self.advance()?;
                }
                _ => break,
            }
        }

        self.expect(Token::EndChoice)?;
        self.skip_newlines()?;

        Ok(Choice {
            name,
            prompt,
            symbol_type,
            default,
            depends,
            options,
        })
    }

    fn parse_menu(&mut self) -> Result<Menu> {
        self.advance()?; // consume 'menu'
        let title = self.parse_string()?;
        self.skip_newlines()?;

        let mut depends = None;
        let mut visible = None;

        // Parse menu attributes
        while matches!(
            self.current_context().current_token,
            Token::Depends | Token::Visible
        ) {
            match &self.current_context().current_token {
                Token::Depends => {
                    self.advance()?;
                    self.expect(Token::On)?;
                    depends = Some(self.parse_expr()?);
                    self.skip_newlines()?;
                }
                Token::Visible => {
                    self.advance()?;
                    self.expect(Token::If)?;
                    visible = Some(self.parse_expr()?);
                    self.skip_newlines()?;
                }
                _ => break,
            }
        }

        let entries = self.parse_entries()?;

        self.expect(Token::EndMenu)?;
        self.skip_newlines()?;

        Ok(Menu {
            title,
            depends,
            visible,
            entries,
        })
    }

    fn parse_if(&mut self) -> Result<If> {
        self.advance()?; // consume 'if'
        let condition = self.parse_expr()?;
        self.skip_newlines()?;

        let entries = self.parse_entries()?;

        self.expect(Token::EndIf)?;
        self.skip_newlines()?;

        Ok(If { condition, entries })
    }

    fn parse_comment(&mut self) -> Result<Comment> {
        self.advance()?; // consume 'comment'
        let text = self.parse_string()?;
        self.skip_newlines()?;

        let mut depends = None;
        if matches!(self.current_context().current_token, Token::Depends) {
            self.advance()?;
            self.expect(Token::On)?;
            depends = Some(self.parse_expr()?);
            self.skip_newlines()?;
        }

        Ok(Comment { text, depends })
    }

    fn parse_expr(&mut self) -> Result<Expr> {
        self.parse_or_expr()
    }

    fn parse_range_bound(&mut self) -> Result<Expr> {
        match &self.current_context().current_token.clone() {
            Token::Identifier(name) => {
                let name = name.clone();
                self.advance()?;
                if name.starts_with("0x") || name.starts_with("0X") {
                    Ok(Expr::Const(name))
                } else {
                    Ok(Expr::Symbol(name))
                }
            }
            Token::Number(n) => {
                let n = *n;
                self.advance()?;
                Ok(Expr::Const(n.to_string()))
            }
            _ => Err(KconfigError::Syntax {
                file: self.current_file.clone(),
                line: self.current_context().lexer.current_line(),
                message: "range bounds must be a symbol reference or numeric literal".to_string(),
            }),
        }
    }

    fn parse_or_expr(&mut self) -> Result<Expr> {
        let mut left = self.parse_and_expr()?;

        while matches!(self.current_context().current_token, Token::Or) {
            self.advance()?;
            let right = self.parse_and_expr()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }

        Ok(left)
    }

    fn parse_and_expr(&mut self) -> Result<Expr> {
        let mut left = self.parse_comparison_expr()?;

        while matches!(self.current_context().current_token, Token::And) {
            self.advance()?;
            let right = self.parse_comparison_expr()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }

        Ok(left)
    }

    fn parse_comparison_expr(&mut self) -> Result<Expr> {
        let left = self.parse_unary_expr()?;

        match &self.current_context().current_token {
            Token::Eq => {
                self.advance()?;
                let right = self.parse_unary_expr()?;
                Ok(Expr::Equal(Box::new(left), Box::new(right)))
            }
            Token::NotEq => {
                self.advance()?;
                let right = self.parse_unary_expr()?;
                Ok(Expr::NotEqual(Box::new(left), Box::new(right)))
            }
            Token::Less => {
                self.advance()?;
                let right = self.parse_unary_expr()?;
                Ok(Expr::Less(Box::new(left), Box::new(right)))
            }
            Token::LessEq => {
                self.advance()?;
                let right = self.parse_unary_expr()?;
                Ok(Expr::LessEqual(Box::new(left), Box::new(right)))
            }
            Token::Greater => {
                self.advance()?;
                let right = self.parse_unary_expr()?;
                Ok(Expr::Greater(Box::new(left), Box::new(right)))
            }
            Token::GreaterEq => {
                self.advance()?;
                let right = self.parse_unary_expr()?;
                Ok(Expr::GreaterEqual(Box::new(left), Box::new(right)))
            }
            _ => Ok(left),
        }
    }

    fn parse_unary_expr(&mut self) -> Result<Expr> {
        if matches!(self.current_context().current_token, Token::Not) {
            self.advance()?;
            let expr = self.parse_unary_expr()?;
            return Ok(Expr::Not(Box::new(expr)));
        }

        self.parse_primary_expr()
    }

    fn parse_primary_expr(&mut self) -> Result<Expr> {
        match &self.current_context().current_token.clone() {
            Token::Identifier(name) => {
                let name = name.clone();
                self.advance()?;
                Ok(Expr::Symbol(name))
            }
            Token::StringLit(val) => {
                let val = val.clone();
                self.advance()?;
                // Check if it contains shell expressions
                if val.contains("$(") {
                    Ok(Expr::ShellExpr(val))
                } else {
                    Ok(Expr::Const(val))
                }
            }
            Token::Number(n) => {
                let n = *n;
                self.advance()?;
                Ok(Expr::Const(n.to_string()))
            }
            Token::LParen => {
                self.advance()?;
                let expr = self.parse_expr()?;
                self.expect(Token::RParen)?;
                Ok(expr)
            }
            _ => Err(KconfigError::Syntax {
                file: self.current_file.clone(),
                line: self.current_context().lexer.current_line(),
                message: format!(
                    "Expected expression, got {:?}",
                    self.current_context().current_token
                ),
            }),
        }
    }

    fn parse_string(&mut self) -> Result<String> {
        match &self.current_context().current_token {
            Token::StringLit(s) => {
                let s = s.clone();
                self.advance()?;
                Ok(s)
            }
            Token::Identifier(s) => {
                let s = s.clone();
                self.advance()?;
                Ok(s)
            }
            _ => Err(KconfigError::Syntax {
                file: self.current_file.clone(),
                line: self.current_context().lexer.current_line(),
                message: format!(
                    "Expected string, got {:?}",
                    self.current_context().current_token
                ),
            }),
        }
    }

    fn parse_identifier(&mut self) -> Result<String> {
        match &self.current_context().current_token {
            Token::Identifier(s) => {
                let s = s.clone();
                self.advance()?;
                Ok(s)
            }
            _ => Err(KconfigError::Syntax {
                file: self.current_file.clone(),
                line: self.current_context().lexer.current_line(),
                message: format!(
                    "Expected identifier, got {:?}",
                    self.current_context().current_token
                ),
            }),
        }
    }

    fn try_parse_prompt(&mut self) -> Result<String> {
        if matches!(
            self.current_context().current_token,
            Token::StringLit(_) | Token::Identifier(_)
        ) {
            self.parse_string()
        } else {
            Err(KconfigError::Parse("No prompt found".to_string()))
        }
    }

    fn parse_array_literal(&mut self) -> Result<Expr> {
        self.expect(Token::LBracket)?;

        let mut elements = Vec::new();
        let mut content = String::from("[");

        loop {
            self.skip_newlines()?;

            // Check if we've reached the end of the array
            match &self.current_context().current_token {
                Token::RBracket => {
                    self.advance()?; // consume ]
                    content.push(']');
                    break;
                }
                Token::Eof => {
                    return Err(KconfigError::Syntax {
                        file: self.current_file.clone(),
                        line: self.current_context().lexer.current_line(),
                        message: "Unexpected EOF in array literal".to_string(),
                    });
                }
                _ => {}
            }

            // Parse array element
            let element = self.parse_array_element()?;
            if !content.ends_with('[') {
                content.push_str(", ");
            }
            content.push_str(&element);
            elements.push(element);

            // Check for comma or end of array
            self.skip_newlines()?;
            if matches!(self.current_context().current_token, Token::Comma) {
                self.advance()?; // consume comma
                // Continue to next element
            }
            // Allow whitespace-separated elements (no comma required)
        }

        // Return the entire array as a Const expression
        Ok(Expr::Const(content))
    }

    fn parse_array_element(&mut self) -> Result<String> {
        let token = self.current_context().current_token.clone();

        match token {
            Token::Number(n) => {
                self.advance()?;
                Ok(n.to_string())
            }
            Token::StringLit(s) => {
                self.advance()?;
                Ok(format!("\"{}\"", s))
            }
            Token::Identifier(s) => {
                self.advance()?;
                // Handle hex numbers or other identifiers
                Ok(s)
            }
            _ => Err(KconfigError::Syntax {
                file: self.current_file.clone(),
                line: self.current_context().lexer.current_line(),
                message: format!("Unexpected token in array: {:?}", token),
            }),
        }
    }

    /// Parse a rangetype type annotation, e.g. `[(u64, u64)]`, `[u32]`, `["&str"]`.
    fn parse_range_type_annotation(&mut self) -> Result<RangeType> {
        self.expect(Token::LBracket)?;

        match self.current_context().current_token.clone() {
            Token::RBracket => {
                return Err(KconfigError::Syntax {
                    file: self.current_file.clone(),
                    line: self.current_context().lexer.current_line(),
                    message: "Empty array type annotation: must specify element type, e.g. [(u64, \
                              u64)]"
                        .to_string(),
                });
            }
            Token::LParen => {
                // Tuple type: [(type1, type2, ...)]
                self.advance()?;
                let mut types = Vec::new();
                loop {
                    types.push(self.parse_rust_type()?);
                    match self.current_context().current_token.clone() {
                        Token::Comma => {
                            self.advance()?;
                        }
                        Token::RParen => break,
                        other => {
                            return Err(KconfigError::Syntax {
                                file: self.current_file.clone(),
                                line: self.current_context().lexer.current_line(),
                                message: format!(
                                    "Expected ',' or ')' in tuple type annotation, got {:?}",
                                    other
                                ),
                            });
                        }
                    }
                }
                self.expect(Token::RParen)?;
                self.expect(Token::RBracket)?;
                Ok(RangeType::Tuple(types))
            }
            Token::StringLit(s) if s == "&str" || s == "String" => {
                self.advance()?;
                self.expect(Token::RBracket)?;
                Ok(RangeType::StringArray)
            }
            _ => {
                // Primitive type: [u32], [usize], etc.
                let ty = self.parse_rust_type()?;
                self.expect(Token::RBracket)?;
                Ok(RangeType::Primitive(ty))
            }
        }
    }

    /// Parse a single Rust type token (used inside rangetype type annotations).
    fn parse_rust_type(&mut self) -> Result<RustType> {
        let token = self.current_context().current_token.clone();
        let ty = match &token {
            Token::U8 => RustType::U8,
            Token::U16 => RustType::U16,
            Token::U32 => RustType::U32,
            Token::U64 => RustType::U64,
            Token::U128 => RustType::U128,
            Token::Usize => RustType::Usize,
            Token::I8 => RustType::I8,
            Token::I16 => RustType::I16,
            Token::I32 => RustType::I32,
            Token::I64 => RustType::I64,
            Token::I128 => RustType::I128,
            Token::Isize => RustType::Isize,
            other => {
                return Err(KconfigError::Syntax {
                    file: self.current_file.clone(),
                    line: self.current_context().lexer.current_line(),
                    message: format!("Expected Rust type (u8, u32, usize, …), got {:?}", other),
                });
            }
        };
        self.advance()?;
        Ok(ty)
    }
}
