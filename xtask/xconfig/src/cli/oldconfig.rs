// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    collections::HashSet,
    io::{self, BufRead, BufReader, Write},
    path::PathBuf,
};

use crate::{
    config::ConfigEngine,
    error::Result,
    kconfig::{
        Config, Entry, MenuConfig, SymbolTable, SymbolType, ast::Choice, expr::evaluate_expr,
    },
};

pub fn oldconfig_command(
    config: PathBuf,
    kconfig: PathBuf,
    srctree: PathBuf,
    auto_defaults: bool,
) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = BufReader::new(stdin.lock());
    let mut output = stdout.lock();
    oldconfig_command_with_io(
        config,
        kconfig,
        srctree,
        auto_defaults,
        &mut input,
        &mut output,
    )
}

pub fn oldconfig_command_with_io<R: BufRead, W: Write>(
    config: PathBuf,
    kconfig: PathBuf,
    srctree: PathBuf,
    auto_defaults: bool,
    input: &mut R,
    output: &mut W,
) -> Result<()> {
    writeln!(output, "Loading existing configuration...")?;
    writeln!(output, "Config: {}", config.display())?;
    writeln!(output, "Kconfig: {}", kconfig.display())?;

    let mut engine = ConfigEngine::from_kconfig(&kconfig, &srctree)?;
    let changes = engine.load_existing_config(&config)?;

    if changes.has_changes() {
        writeln!(output)?;
        changes.write_summary(output)?;
    } else {
        writeln!(output, "✅ No configuration changes detected.")?;
    }

    if auto_defaults {
        writeln!(output, "\nApplying default values to new symbols...")?;
        engine.refresh_prompt_state();
    } else {
        writeln!(output, "\nPrompting for new configuration options...")?;
        run_oldconfig_prompts(&mut engine, input, output)?;
    }

    writeln!(output, "\nSaving configuration to {}...", config.display())?;
    engine.write_config(&config)?;
    writeln!(output, "✅ Configuration saved successfully.")?;

    Ok(())
}

#[derive(Clone)]
enum PromptSpec {
    Config(ConfigPrompt),
    Choice(ChoicePrompt),
}

impl PromptSpec {
    fn key(&self) -> &str {
        match self {
            Self::Config(prompt) => &prompt.symbol,
            Self::Choice(prompt) => &prompt.key,
        }
    }
}

#[derive(Clone)]
struct ConfigPrompt {
    symbol: String,
    prompt: String,
    symbol_type: SymbolType,
    help: Option<String>,
}

#[derive(Clone)]
struct ChoicePrompt {
    key: String,
    prompt: String,
    options: Vec<ChoiceOption>,
    default_symbol: String,
}

#[derive(Clone)]
struct ChoiceOption {
    symbol: String,
    prompt: String,
    help: Option<String>,
}

fn run_oldconfig_prompts<R: BufRead, W: Write>(
    engine: &mut ConfigEngine,
    input: &mut R,
    output: &mut W,
) -> Result<()> {
    let mut asked = HashSet::new();

    while let Some(prompt) = next_prompt(engine, &asked) {
        match &prompt {
            PromptSpec::Config(config_prompt) => {
                prompt_for_config(engine, config_prompt, input, output)?;
            }
            PromptSpec::Choice(choice_prompt) => {
                prompt_for_choice(engine, choice_prompt, input, output)?;
            }
        }

        asked.insert(prompt.key().to_string());
        engine.refresh_prompt_state();
    }

    Ok(())
}

fn next_prompt(engine: &ConfigEngine, asked: &HashSet<String>) -> Option<PromptSpec> {
    find_next_prompt(engine.entries(), engine.symbols(), asked)
}

fn find_next_prompt(
    entries: &[Entry],
    symbols: &SymbolTable,
    asked: &HashSet<String>,
) -> Option<PromptSpec> {
    for entry in entries {
        match entry {
            Entry::Config(config) => {
                if let Some(prompt) = prompt_for_config_entry(config, symbols, asked) {
                    return Some(PromptSpec::Config(prompt));
                }
            }
            Entry::MenuConfig(menuconfig) => {
                if let Some(prompt) = prompt_for_menuconfig_entry(menuconfig, symbols, asked) {
                    return Some(PromptSpec::Config(prompt));
                }
            }
            Entry::Choice(choice) => {
                if let Some(prompt) = prompt_for_choice_entry(choice, symbols, asked) {
                    return Some(PromptSpec::Choice(prompt));
                }
            }
            Entry::Menu(menu) => {
                if expr_is_visible(menu.depends.as_ref(), symbols)
                    && expr_is_visible(menu.visible.as_ref(), symbols)
                {
                    if let Some(prompt) = find_next_prompt(&menu.entries, symbols, asked) {
                        return Some(prompt);
                    }
                }
            }
            Entry::If(if_entry) => {
                if evaluate_expr(&if_entry.condition, symbols).unwrap_or(false) {
                    if let Some(prompt) = find_next_prompt(&if_entry.entries, symbols, asked) {
                        return Some(prompt);
                    }
                }
            }
            _ => {}
        }
    }

    None
}

fn prompt_for_config_entry(
    config: &Config,
    symbols: &SymbolTable,
    asked: &HashSet<String>,
) -> Option<ConfigPrompt> {
    let symbol = clean_symbol_name(&config.name);
    let symbol_state = symbols.get_symbol(&symbol)?;
    if !symbol_state.is_new
        || asked.contains(&symbol)
        || !expr_is_visible(config.properties.depends.as_ref(), symbols)
    {
        return None;
    }

    Some(ConfigPrompt {
        symbol,
        prompt: config
            .properties
            .prompt
            .clone()
            .unwrap_or_else(|| clean_symbol_name(&config.name)),
        symbol_type: config.symbol_type.clone(),
        help: config.properties.help.clone(),
    })
}

fn prompt_for_menuconfig_entry(
    menuconfig: &MenuConfig,
    symbols: &SymbolTable,
    asked: &HashSet<String>,
) -> Option<ConfigPrompt> {
    let symbol = clean_symbol_name(&menuconfig.name);
    let symbol_state = symbols.get_symbol(&symbol)?;
    if !symbol_state.is_new
        || asked.contains(&symbol)
        || !expr_is_visible(menuconfig.properties.depends.as_ref(), symbols)
    {
        return None;
    }

    Some(ConfigPrompt {
        symbol,
        prompt: menuconfig
            .properties
            .prompt
            .clone()
            .unwrap_or_else(|| clean_symbol_name(&menuconfig.name)),
        symbol_type: menuconfig.symbol_type.clone(),
        help: menuconfig.properties.help.clone(),
    })
}

fn prompt_for_choice_entry(
    choice: &Choice,
    symbols: &SymbolTable,
    asked: &HashSet<String>,
) -> Option<ChoicePrompt> {
    if !expr_is_visible(choice.depends.as_ref(), symbols) {
        return None;
    }

    let key = choice_prompt_key(choice);
    if asked.contains(&key) {
        return None;
    }

    let mut options = Vec::new();
    let mut has_new_option = false;
    let mut has_existing_selection = false;
    let mut current_selection = None;

    for option in &choice.options {
        if !expr_is_visible(option.properties.depends.as_ref(), symbols) {
            continue;
        }

        let symbol = clean_symbol_name(&option.name);
        let symbol_state = symbols.get_symbol(&symbol)?;
        has_new_option |= symbol_state.is_new;
        has_existing_selection |=
            symbol_state.from_config && symbol_state.value.as_deref() == Some("y");
        if symbol_state.value.as_deref() == Some("y") {
            current_selection = Some(symbol.clone());
        }

        options.push(ChoiceOption {
            symbol,
            prompt: option
                .properties
                .prompt
                .clone()
                .unwrap_or_else(|| clean_symbol_name(&option.name)),
            help: option.properties.help.clone(),
        });
    }

    if !has_new_option || has_existing_selection || options.is_empty() {
        return None;
    }

    let default_symbol = current_selection.unwrap_or_else(|| {
        choice
            .default
            .as_deref()
            .map(clean_symbol_name)
            .filter(|name| options.iter().any(|option| option.symbol == *name))
            .unwrap_or_else(|| options[0].symbol.clone())
    });

    Some(ChoicePrompt {
        key,
        prompt: choice
            .prompt
            .clone()
            .or_else(|| choice.name.clone())
            .unwrap_or_else(|| "Choice".to_string()),
        options,
        default_symbol,
    })
}

fn prompt_for_config<R: BufRead, W: Write>(
    engine: &mut ConfigEngine,
    prompt: &ConfigPrompt,
    input: &mut R,
    output: &mut W,
) -> Result<()> {
    loop {
        write!(
            output,
            "{} ({}) {} ",
            prompt.prompt,
            prompt.symbol,
            prompt_suffix(&prompt.symbol_type, engine.get_value(&prompt.symbol))
        )?;
        output.flush()?;

        let response = read_trimmed_line(input)?;
        if response == "?" {
            write_help(output, &prompt.help)?;
            continue;
        }

        let Some(value) = parse_prompt_value(prompt, &response, engine.get_value(&prompt.symbol))
        else {
            writeln!(output, "Invalid input, please try again.")?;
            continue;
        };

        if apply_prompt_value(engine, &prompt.symbol, &prompt.symbol_type, value, output)? {
            break;
        }
    }

    Ok(())
}

fn prompt_for_choice<R: BufRead, W: Write>(
    engine: &mut ConfigEngine,
    prompt: &ChoicePrompt,
    input: &mut R,
    output: &mut W,
) -> Result<()> {
    loop {
        writeln!(output, "{}:", prompt.prompt)?;
        for (index, option) in prompt.options.iter().enumerate() {
            let marker = if option.symbol == prompt.default_symbol {
                " (default)"
            } else {
                ""
            };
            writeln!(
                output,
                "  {}. {} ({}){}",
                index + 1,
                option.prompt,
                option.symbol,
                marker
            )?;
        }
        write!(
            output,
            "Select choice [1-{} or Enter for default, ? for help] ",
            prompt.options.len()
        )?;
        output.flush()?;

        let response = read_trimmed_line(input)?;
        if response == "?" {
            for option in &prompt.options {
                writeln!(output, "- {} ({})", option.prompt, option.symbol)?;
                write_help(output, &option.help)?;
            }
            continue;
        }

        let selected_symbol = if response.is_empty() {
            prompt.default_symbol.clone()
        } else if let Ok(index) = response.parse::<usize>() {
            let Some(option) = prompt.options.get(index.saturating_sub(1)) else {
                writeln!(output, "Invalid choice, please try again.")?;
                continue;
            };
            option.symbol.clone()
        } else {
            writeln!(output, "Invalid choice, please try again.")?;
            continue;
        };

        if let Err(err) = engine.can_enable(&selected_symbol) {
            writeln!(output, "{err}")?;
            continue;
        }

        for option in &prompt.options {
            let value = if option.symbol == selected_symbol {
                "y"
            } else {
                "n"
            };
            engine.set_value(&option.symbol, value.to_string());
        }
        let selected = engine.apply_selects(&selected_symbol);
        if !selected.is_empty() {
            writeln!(output, "Enabled via select: {}", selected.join(", "))?;
        }
        break;
    }

    Ok(())
}

fn apply_prompt_value<W: Write>(
    engine: &mut ConfigEngine,
    symbol: &str,
    symbol_type: &SymbolType,
    value: Option<String>,
    output: &mut W,
) -> Result<bool> {
    match symbol_type {
        SymbolType::Bool | SymbolType::Tristate => {
            let new_value = value.unwrap_or_else(|| "n".to_string());
            if new_value == "n" {
                if let Err(err) = engine.can_disable(symbol) {
                    writeln!(output, "{err}")?;
                    return Ok(false);
                }
            } else if let Err(err) = engine.can_enable(symbol) {
                writeln!(output, "{err}")?;
                return Ok(false);
            }

            engine.set_value(symbol, new_value.clone());
            if new_value != "n" {
                let selected = engine.apply_selects(symbol);
                if !selected.is_empty() {
                    writeln!(output, "Enabled via select: {}", selected.join(", "))?;
                }
            }
        }
        _ => {
            if let Some(new_value) = value {
                engine.set_value(symbol, new_value);
            } else {
                engine.symbols_mut().clear_value(symbol);
            }
        }
    }

    Ok(true)
}

fn parse_prompt_value(
    prompt: &ConfigPrompt,
    response: &str,
    current: Option<String>,
) -> Option<Option<String>> {
    let trimmed = response.trim();
    match prompt.symbol_type {
        SymbolType::Bool => parse_bool_answer(trimmed, current),
        SymbolType::Tristate => parse_tristate_answer(trimmed, current),
        SymbolType::String => {
            if trimmed.is_empty() {
                Some(current)
            } else {
                Some(Some(trimmed.to_string()))
            }
        }
        SymbolType::Hex => parse_hex_answer(trimmed, current),
        SymbolType::Range(_) => parse_range_answer(trimmed, current),
        ref ty if ty.is_integer_type() => parse_integer_answer(ty, trimmed, current),
        _ => {
            if trimmed.is_empty() {
                Some(current)
            } else {
                Some(Some(trimmed.to_string()))
            }
        }
    }
}

fn parse_bool_answer(input: &str, current: Option<String>) -> Option<Option<String>> {
    if input.is_empty() {
        return Some(current.or(Some("n".to_string())));
    }

    match input.to_ascii_lowercase().as_str() {
        "y" | "yes" => Some(Some("y".to_string())),
        "n" | "no" => Some(Some("n".to_string())),
        _ => None,
    }
}

fn parse_tristate_answer(input: &str, current: Option<String>) -> Option<Option<String>> {
    if input.is_empty() {
        return Some(current.or(Some("n".to_string())));
    }

    match input.to_ascii_lowercase().as_str() {
        "y" | "yes" => Some(Some("y".to_string())),
        "m" | "mod" | "module" => Some(Some("m".to_string())),
        "n" | "no" => Some(Some("n".to_string())),
        _ => None,
    }
}

fn parse_integer_answer(
    symbol_type: &SymbolType,
    input: &str,
    current: Option<String>,
) -> Option<Option<String>> {
    if input.is_empty() {
        return Some(current);
    }

    let valid = match symbol_type {
        SymbolType::U8 => input.parse::<u8>().is_ok(),
        SymbolType::U16 => input.parse::<u16>().is_ok(),
        SymbolType::U32 => input.parse::<u32>().is_ok(),
        SymbolType::U64 => input.parse::<u64>().is_ok(),
        SymbolType::U128 => input.parse::<u128>().is_ok(),
        SymbolType::Usize => input.parse::<usize>().is_ok(),
        SymbolType::I8 => input.parse::<i8>().is_ok(),
        SymbolType::I16 => input.parse::<i16>().is_ok(),
        SymbolType::I32 => input.parse::<i32>().is_ok(),
        SymbolType::I64 => input.parse::<i64>().is_ok(),
        SymbolType::I128 => input.parse::<i128>().is_ok(),
        SymbolType::Isize => input.parse::<isize>().is_ok(),
        _ => false,
    };

    valid.then(|| Some(input.to_string()))
}

fn parse_hex_answer(input: &str, current: Option<String>) -> Option<Option<String>> {
    if input.is_empty() {
        return Some(current);
    }

    let normalized = input.trim();
    let valid = if normalized.starts_with("0x") || normalized.starts_with("0X") {
        u64::from_str_radix(&normalized[2..], 16).is_ok()
    } else {
        normalized.parse::<u64>().is_ok()
    };

    valid.then(|| Some(normalized.to_string()))
}

fn parse_range_answer(input: &str, current: Option<String>) -> Option<Option<String>> {
    if input.is_empty() {
        return Some(current);
    }

    let trimmed = input.trim();
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        Some(Some(trimmed.to_string()))
    } else {
        None
    }
}

fn prompt_suffix(symbol_type: &SymbolType, current: Option<String>) -> String {
    match symbol_type {
        SymbolType::Bool => match current.as_deref().unwrap_or("n") {
            "y" => "[Y/n/?]".to_string(),
            _ => "[N/y/?]".to_string(),
        },
        SymbolType::Tristate => {
            format!("[y/m/n/?] (default: {})", current.as_deref().unwrap_or("n"))
        }
        _ => format!("[default: {}]", current.as_deref().unwrap_or("")),
    }
}

fn write_help<W: Write>(output: &mut W, help: &Option<String>) -> Result<()> {
    match help {
        Some(help_text) => writeln!(output, "{help_text}")?,
        None => writeln!(output, "No help available.")?,
    }
    Ok(())
}

fn read_trimmed_line<R: BufRead>(input: &mut R) -> Result<String> {
    let mut line = String::new();
    input.read_line(&mut line)?;
    Ok(line.trim().to_string())
}

fn expr_is_visible(expr: Option<&crate::kconfig::ast::Expr>, symbols: &SymbolTable) -> bool {
    expr.map(|condition| evaluate_expr(condition, symbols).unwrap_or(false))
        .unwrap_or(true)
}

fn clean_symbol_name(name: &str) -> String {
    name.strip_prefix("CONFIG_").unwrap_or(name).to_string()
}

fn choice_prompt_key(choice: &Choice) -> String {
    if let Some(name) = &choice.name {
        format!("choice:{name}")
    } else {
        let option_names = choice
            .options
            .iter()
            .map(|option| clean_symbol_name(&option.name))
            .collect::<Vec<_>>()
            .join("|");
        format!("choice:{option_names}")
    }
}
