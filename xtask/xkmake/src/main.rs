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
mod x86;

use clap::Parser;

use crate::{
    cli::{Cli, Command, HygieneCommand},
    error::Result,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
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
            qemu::run(&bundle, &args)?;
            coverage::generate(&bundle, &args)
        }
        Command::Config(args) => context::print_config(&args),
        Command::Hygiene(args) => match args.command {
            HygieneCommand::InstallTools => hygiene::install_tools(),
            HygieneCommand::Deps(args) => hygiene::check_dependencies(&args),
            HygieneCommand::Header(args) => hygiene::check_headers(&args),
        },
    }
}
