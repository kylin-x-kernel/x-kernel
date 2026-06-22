// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
};

use clap::Parser;
use rsext4::{Jbd2Dev, config::BLOCK_SIZE, ext4::Ext4FileSystem, file::read_file};

mod blockdev;
use blockdev::FileBlockDev;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to the ext4 image file
    #[arg(short, long)]
    image: PathBuf,

    /// Relative path under `/.llvm-cov/` or an absolute path already rooted there
    #[arg(short, long, default_value = "/.llvm-cov/default.profraw")]
    profraw_path: String,

    /// Path to write the extracted file
    #[arg(short, long, default_value = "default.profraw")]
    out_path: PathBuf,
}

fn main() -> Result<(), String> {
    let args = Args::parse();

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&args.image)
        .map_err(|e| format!("failed to open image {}: {}", args.image.display(), e))?;

    let meta = file.metadata().map_err(|e| e.to_string())?;
    let total_blocks = meta.len() / (BLOCK_SIZE as u64);

    let dev = FileBlockDev::new(file, total_blocks, BLOCK_SIZE as u32);
    let mut jbd = Jbd2Dev::initial_jbd2dev(0, dev, false);

    let mut fs = Ext4FileSystem::mount(&mut jbd).map_err(|e| format!("mount failed: {}", e))?;

    let profraw_path = sanitize_profraw_path(&args.profraw_path)?;
    let data = read_file(&mut jbd, &mut fs, &profraw_path)
        .map_err(|e| format!("failed to read file from ext4: {:?}", e))?
        .ok_or_else(|| format!("file {} not found in ext4 image", profraw_path))?;

    let out_file_path = args.out_path;

    let mut out_file = File::create(&out_file_path).map_err(|e| e.to_string())?;
    out_file.write_all(&data).map_err(|e| e.to_string())?;

    println!(
        "Successfully extracted {} to {}",
        profraw_path,
        out_file_path.display()
    );

    Ok(())
}

fn sanitize_profraw_path(input: &str) -> Result<String, String> {
    const COVERAGE_ROOT: &str = ".llvm-cov";

    let mut components = Path::new(input).components().peekable();
    let mut saw_root = false;
    if matches!(components.peek(), Some(Component::RootDir)) {
        saw_root = true;
        components.next();
    }

    let mut relative_path = PathBuf::new();
    for component in components {
        match component {
            Component::Normal(part) => relative_path.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "invalid --profraw-path {input:?}: path must stay under /.llvm-cov/"
                ));
            }
        }
    }

    if relative_path.as_os_str().is_empty() {
        return Err("invalid --profraw-path: path must not be empty".to_string());
    }

    let mut parts = relative_path.components();
    let normalized = if saw_root {
        let Some(Component::Normal(root)) = parts.next() else {
            return Err(format!(
                "invalid --profraw-path {input:?}: absolute path must stay under /.llvm-cov/"
            ));
        };
        if root != COVERAGE_ROOT {
            return Err(format!(
                "invalid --profraw-path {input:?}: absolute path must stay under /.llvm-cov/"
            ));
        }

        let trimmed = parts.as_path();
        if trimmed.as_os_str().is_empty() {
            return Err(format!(
                "invalid --profraw-path {input:?}: path must name a file under /.llvm-cov/"
            ));
        }
        trimmed.to_path_buf()
    } else {
        relative_path
    };

    Ok(format!("/.llvm-cov/{}", normalized.display()))
}

#[cfg(test)]
mod tests {
    use super::sanitize_profraw_path;

    #[test]
    fn sanitize_profraw_path_accepts_relative_paths() {
        assert_eq!(
            sanitize_profraw_path("case/default.profraw").unwrap(),
            "/.llvm-cov/case/default.profraw"
        );
    }

    #[test]
    fn sanitize_profraw_path_rejects_parent_traversal() {
        assert!(sanitize_profraw_path("../etc/passwd").is_err());
    }

    #[test]
    fn sanitize_profraw_path_rejects_absolute_paths() {
        assert!(sanitize_profraw_path("/etc/passwd").is_err());
    }

    #[test]
    fn sanitize_profraw_path_accepts_legacy_absolute_coverage_paths() {
        assert_eq!(
            sanitize_profraw_path("/.llvm-cov/default.profraw").unwrap(),
            "/.llvm-cov/default.profraw"
        );
    }

    #[test]
    fn sanitize_profraw_path_rejects_empty_paths() {
        assert!(sanitize_profraw_path("").is_err());
        assert!(sanitize_profraw_path(".").is_err());
        assert!(sanitize_profraw_path("/.llvm-cov").is_err());
    }
}
