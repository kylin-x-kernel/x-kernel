// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel build and run orchestration.

mod build;
mod cli;
mod context;
mod coverage;
mod doc;
mod dwarf_embed;
mod error;
mod hygiene;
mod image_metadata;
mod linker;
mod process;
mod qemu;
mod symbolize;
mod symtab;
mod x86;

use clap::Parser;
use env_logger::{Builder, Env};

use crate::{
    cli::{Cli, Command, HygieneCommand},
    error::Result,
};

fn main() {
    // Initialize the logger before any work begins.
    //
    // The format is `warning:` / `error:` (lowercase, like `cargo`) with only
    // the prefix colored when stderr is a terminal; no timestamp or module
    // target, so the output stays compact. `Builder::from_env` (rather than
    // `Builder::new`) is used so that `RUST_LOG` overrides level/per-module
    // filtering and `RUST_LOG_STYLE` overrides coloring for debugging; the
    // default floor is `info` so warnings and errors are always shown.
    Builder::from_env(Env::default().default_filter_or("info"))
        .format(|buf, record| {
            use std::io::Write;
            let prefix = match record.level() {
                log::Level::Error => "error",
                log::Level::Warn => "warning",
                log::Level::Info => "info",
                log::Level::Debug => "debug",
                log::Level::Trace => "trace",
            };
            let style = buf.default_level_style(record.level());
            writeln!(
                buf,
                "{}{}:{} {}",
                style.render(),
                prefix,
                style.render_reset(),
                record.args()
            )
        })
        .init();

    if let Err(error) = run() {
        log::error!("{error}");
        std::process::exit(error.exit_code());
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Build(args) => {
            let bundle = build::build(&args)?;
            println!("Built {}", bundle.directory.display());
            Ok(())
        }
        Command::Clippy(args) => build::clippy(&args),
        Command::Doc(args) => doc::generate(&args),
        Command::Run(args) => {
            let bundle = if args.no_build {
                build::existing_bundle(&args.build)?
            } else {
                build::build(&args.build)?
            };
            let qemu_result = qemu::run(&bundle, &args);
            if !args.no_symbolize {
                // Symbolicate any panic backtrace from the QEMU log; failures
                // degrade to warnings and never mask the QEMU result.
                if let Err(error) = symbolize::auto(&bundle, &qemu::log_path(&bundle)) {
                    eprintln!("[xkmake] auto symbolication failed: {error}");
                }
            }
            qemu_result?;
            coverage::generate(&bundle, &args)
        }
        Command::Config(args) => context::print_config(&args),
        Command::Symbolize(args) => symbolize::run(&args),
        Command::Hygiene(args) => match args.command {
            HygieneCommand::InstallTools => hygiene::install_tools(),
            HygieneCommand::Deps(args) => hygiene::check_dependencies(&args),
            HygieneCommand::Header(args) => hygiene::check_headers(&args),
        },
    }
}
