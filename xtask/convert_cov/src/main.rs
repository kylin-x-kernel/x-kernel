// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    error::Error,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use clap::Parser;
use lcov2cobertura::{RustDemangler, coverage_to_file, parse_file};

#[derive(Parser, Debug)]
#[command(author, version, about = "Convert LCOV tracefile to Cobertura XML")]
struct Args {
    /// Path to the LCOV tracefile.
    input: PathBuf,

    /// Path to the generated Cobertura XML file.
    output: PathBuf,

    /// Base directory used to strip source prefixes in the XML output.
    #[arg(long, default_value = ".")]
    base_dir: PathBuf,

    /// Regex for excluding packages from the generated report.
    #[arg(long = "exclude")]
    excludes: Vec<String>,
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    let excludes = args.excludes.iter().map(String::as_str).collect::<Vec<_>>();
    let coverage = parse_file(&args.input, &args.base_dir, &excludes)?;
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

    coverage_to_file(&args.output, &coverage, timestamp, RustDemangler::new())?;

    println!(
        "Converted {} to {}",
        args.input.display(),
        args.output.display()
    );
    Ok(())
}
