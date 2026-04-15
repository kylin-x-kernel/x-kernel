// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::PathBuf,
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

    /// Path to the file inside the ext4 image
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

    let profraw_path = args.profraw_path.as_str();
    let data = read_file(&mut jbd, &mut fs, profraw_path)
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
