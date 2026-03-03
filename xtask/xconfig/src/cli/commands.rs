// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::path::PathBuf;

use clap::{Parser as ClapParser, Subcommand};

use crate::{
    config::{ConfigGenerator, ConfigReader},
    error::Result,
    kconfig::{Parser, SymbolTable},
};

#[derive(ClapParser, Debug)]
#[command(name = "xconf")]
#[command(about = "Rust Kconfig tool - Kbuild configuration system", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Parse a Kconfig file and display the AST
    Parse {
        /// Path to Kconfig file
        #[arg(short, long, default_value = "Kconfig")]
        kconfig: PathBuf,

        /// Source tree path
        #[arg(short, long, default_value = ".")]
        srctree: PathBuf,
    },

    /// Apply a defconfig
    Defconfig {
        /// Path to defconfig file
        defconfig: PathBuf,

        /// Path to Kconfig file
        #[arg(short, long, default_value = "Kconfig")]
        kconfig: PathBuf,

        /// Source tree path
        #[arg(short, long, default_value = ".")]
        srctree: PathBuf,
    },

    /// Interactive menu configuration (TUI)
    Menuconfig {
        /// Path to Kconfig file
        #[arg(short, long, default_value = "Kconfig")]
        kconfig: PathBuf,

        /// Source tree path
        #[arg(short, long, default_value = ".")]
        srctree: PathBuf,
    },

    /// Generate configuration files
    Generate {
        /// Path to .config file
        #[arg(short, long, default_value = ".config")]
        config: PathBuf,

        /// Path to Kconfig file
        #[arg(short, long, default_value = "Kconfig")]
        kconfig: PathBuf,

        /// Source tree path
        #[arg(short, long, default_value = ".")]
        srctree: PathBuf,
    },

    /// Load an existing .config and detect changes (oldconfig)
    Oldconfig {
        /// Path to existing .config file
        #[arg(short, long, default_value = ".config")]
        config: PathBuf,

        /// Path to Kconfig file
        #[arg(short, long, default_value = "Kconfig")]
        kconfig: PathBuf,

        /// Source tree path
        #[arg(short, long, default_value = ".")]
        srctree: PathBuf,

        /// Automatically apply defaults to new symbols
        #[arg(long)]
        auto_defaults: bool,
    },

    /// Save current configuration
    Saveconfig {
        /// Output path for .config
        #[arg(short, long, default_value = ".config")]
        output: PathBuf,

        /// Path to Kconfig file (to get current symbols)
        #[arg(short, long, default_value = "Kconfig")]
        kconfig: PathBuf,

        /// Source tree path
        #[arg(short, long, default_value = ".")]
        srctree: PathBuf,
    },

    /// Generate Rust const definitions from .config
    GenConst {
        /// Path to .config file
        #[arg(short, long, default_value = ".config")]
        config: PathBuf,

        /// Output directory for generated config.rs
        #[arg(short, long, default_value = "target/kbuild")]
        output_dir: PathBuf,

        /// Path to Kconfig file (source of truth for types)
        #[arg(short, long, default_value = "Kconfig")]
        kconfig: PathBuf,

        /// Source tree path
        #[arg(short, long, default_value = ".")]
        srctree: PathBuf,
    },
}

pub fn parse_command(kconfig: PathBuf, srctree: PathBuf) -> Result<()> {
    println!("Parsing Kconfig file: {}", kconfig.display());
    println!("Source tree: {}", srctree.display());

    let mut parser = Parser::new(&kconfig, &srctree)?;
    let ast = parser.parse()?;

    println!("\nParsed {} entries", ast.entries.len());
    println!("\nAST:");
    for (i, entry) in ast.entries.iter().enumerate() {
        println!("{}: {:?}", i, entry);
    }

    Ok(())
}

pub fn generate_command(config: PathBuf, kconfig: PathBuf, srctree: PathBuf) -> Result<()> {
    println!("Generating configuration files...");
    println!("Config: {}", config.display());
    println!("Kconfig: {}", kconfig.display());

    // Parse Kconfig
    let mut parser = Parser::new(&kconfig, &srctree)?;
    let _ast = parser.parse()?;

    // Read .config
    let config_values = ConfigReader::read(&config)?;

    // Build symbol table
    let mut symbols = SymbolTable::new();
    for (name, value) in config_values {
        // Extract symbol type from name (simplified)
        symbols.add_symbol(name.clone(), crate::kconfig::SymbolType::Bool);
        symbols.set_value(&name, value);
    }

    // Generate auto.conf
    ConfigGenerator::generate_auto_conf("auto.conf", &symbols)?;
    println!("Generated auto.conf");

    // Generate autoconf.h
    ConfigGenerator::generate_autoconf_h("autoconf.h", &symbols)?;
    println!("Generated autoconf.h");

    Ok(())
}

pub fn run_cli() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Parse { kconfig, srctree } => parse_command(kconfig, srctree),
        Commands::Defconfig {
            defconfig,
            kconfig,
            srctree,
        } => crate::cli::defconfig::defconfig_command(defconfig, kconfig, srctree),
        Commands::Menuconfig { kconfig, srctree } => {
            crate::cli::menuconfig::menuconfig_command(kconfig, srctree)
        }
        Commands::Generate {
            config,
            kconfig,
            srctree,
        } => generate_command(config, kconfig, srctree),
        Commands::Oldconfig {
            config,
            kconfig,
            srctree,
            auto_defaults,
        } => crate::cli::oldconfig::oldconfig_command(config, kconfig, srctree, auto_defaults),
        Commands::Saveconfig {
            output,
            kconfig,
            srctree,
        } => crate::cli::saveconfig::saveconfig_command(output, kconfig, srctree),
        Commands::GenConst {
            config,
            output_dir,
            kconfig,
            srctree,
        } => crate::cli::gen_const::gen_const_command(config, output_dir, kconfig, srctree),
    }
}
