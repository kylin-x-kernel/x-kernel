// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

mod app;
mod autostart;
mod cli;
mod debugfs;
mod install;
mod manifest;
mod prepare;

use cli::{Args, Command};

fn main() {
    let args = Args::parse();
    let result = match args.command {
        Command::List(command) => app::list(command),
        Command::Prepare(command) => app::prepare(command),
        Command::Install(command) => app::install(command),
    };

    if let Err(err) = result {
        eprintln!("uapp failed: {err}");
        std::process::exit(1);
    }
}
