// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    error::Error,
    path::PathBuf,
    process::Command,
};

use clap::Parser;

#[derive(Parser, Debug)]
#[command(author, version, about = "Generate workspace documentation")]
struct Args {
    /// Path to the xconfig-generated cargo config.
    #[arg(long, default_value = ".cargo/.xconfig.toml")]
    config: PathBuf,

    /// Verbose level: -v, -vv.
    #[arg(short = 'v', action = clap::ArgAction::Count)]
    verbose: u8,

    /// Extra cargo flags (e.g. --open).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    extra: Vec<String>,
}

fn read_features(path: &PathBuf) -> Result<Vec<String>, Box<dyn Error>> {
    let content = std::fs::read_to_string(path)?;
    let value: toml::Value = toml::from_str(&content)?;
    let features = value["features"]
        .as_array()
        .ok_or("missing `features` array")?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    Ok(features)
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();

    if !args.config.exists() {
        eprintln!(
            "Error: {} not found. Run `make build` first to generate it.",
            args.config.display()
        );
        std::process::exit(1);
    }

    let features = read_features(&args.config)?;
    let features_str = features.join(",");

    let verbose_flag = match args.verbose {
        0 => "",
        1 => "-v",
        _ => "-vv",
    };

    let mut cmd = Command::new("cargo");
    cmd.args(["doc", "--no-deps", "--workspace"])
        .args(["--features", &features_str]);

    if !verbose_flag.is_empty() {
        cmd.arg(verbose_flag);
    }

    for extra in &args.extra {
        cmd.arg(extra);
    }

    println!("> {}", format_command(&cmd));
    let status = cmd.status()?;
    if !status.success() {
        return Err(format!("cargo doc exited with {}", status).into());
    }

    println!("\nGenerated docs. Open target/doc/index.html");
    Ok(())
}

fn format_command(cmd: &Command) -> String {
    let mut s = String::new();
    for arg in cmd.get_args() {
        if !s.is_empty() {
            s.push(' ');
        }
        if arg.to_string_lossy().contains(' ') || arg.to_string_lossy().contains(',') {
            s.push('"');
            s.push_str(&arg.to_string_lossy());
            s.push('"');
        } else {
            s.push_str(&arg.to_string_lossy());
        }
    }
    format!("{} {}", cmd.get_program().to_string_lossy(), s)
}
