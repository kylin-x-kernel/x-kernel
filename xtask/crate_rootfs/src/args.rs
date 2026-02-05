use std::path::PathBuf;

use clap::Parser;

/// Parse human-readable size strings like 64M, 512K, or raw bytes.
fn parse_size(input: &str) -> Result<u64, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("size is empty".to_string());
    }

    let (num_str, unit) = s.split_at(s.len().saturating_sub(1));
    let (value, multiplier) = match unit.to_ascii_lowercase().as_str() {
        "k" => (num_str, 1024u64),
        "m" => (num_str, 1024u64 * 1024),
        "g" => (num_str, 1024u64 * 1024 * 1024),
        _ => (s, 1u64),
    };

    let num: u64 = value
        .parse()
        .map_err(|_| format!("invalid size: {input}"))?;
    Ok(num.saturating_mul(multiplier))
}

#[derive(Debug, Clone)]
pub struct CopySpec {
    pub src: PathBuf,
    pub dest: String,
}

fn parse_copy_spec(input: &str) -> Result<CopySpec, String> {
    let (src, dest) = input
        .split_once(':')
        .ok_or_else(|| "copy spec must be in SRC:DEST format".to_string())?;
    if src.trim().is_empty() || dest.trim().is_empty() {
        return Err("copy spec must be in SRC:DEST format".to_string());
    }
    Ok(CopySpec {
        src: PathBuf::from(src),
        dest: dest.to_string(),
    })
}

/// Command line arguments for rootfs creation.
#[derive(Debug, Parser)]
#[command(author, version, about = "Create an ext4 rootfs image")]
pub struct Args {
    /// Output image path.
    #[arg(long, default_value = "disk.img")]
    pub image: PathBuf,

    /// Image size (bytes or with K/M/G suffix).
    #[arg(long, default_value = "64M", value_parser = parse_size)]
    pub size_bytes: u64,

    /// Multiple copy specs in SRC:DEST format. Can be provided multiple times.
    #[arg(long = "copy", value_parser = parse_copy_spec)]
    pub copies: Vec<CopySpec>,
}

/// Parse CLI args into a structured Args value.
pub fn parse_args() -> Args {
    Args::parse()
}
