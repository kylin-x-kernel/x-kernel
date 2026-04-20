// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{io, path::PathBuf};

use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::{config::ConfigEngine, error::Result, ui::MenuConfigApp};

pub fn menuconfig_command(kconfig: PathBuf, srctree: PathBuf) -> Result<()> {
    println!("Loading configuration...");

    let mut engine = ConfigEngine::from_kconfig(&kconfig, &srctree)?;
    println!("Parsed {} entries", engine.entries().len());

    if std::path::Path::new(".config").exists() {
        println!("Loading existing .config...");
        engine.load_menuconfig_config(".config")?;
    } else {
        println!("No existing .config found, using defaults");
    }

    println!("Launching TUI...");

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let (entries, symbol_table, dependency_resolver) = engine.into_menuconfig_parts();
    let mut app = MenuConfigApp::new_with_resolver(entries, symbol_table, dependency_resolver)?;
    let res = app.run(&mut terminal);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    res
}
