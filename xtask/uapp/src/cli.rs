// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Install uapps into an existing X-Kernel disk image"
)]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
}

impl Args {
    pub fn parse() -> Self {
        <Self as Parser>::parse()
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List discovered uapp manifests.
    List(ListCommand),
    /// Run host-side prepare commands for selected uapps.
    Prepare(PrepareCommand),
    /// Prepare selected uapps and inject them into an existing disk image.
    Install(InstallCommand),
}

#[derive(Debug, Parser)]
pub struct ListCommand {
    /// Directory containing uapp child directories.
    #[arg(long, default_value = "uapps")]
    pub uapps_dir: PathBuf,

    /// Comma-separated uapp selection, or "all".
    #[arg(long, default_value = "all")]
    pub select: String,
}

#[derive(Debug, Parser)]
pub struct PrepareCommand {
    /// Directory containing uapp child directories.
    #[arg(long, default_value = "uapps")]
    pub uapps_dir: PathBuf,

    /// Comma-separated uapp selection, or "all".
    #[arg(long, default_value = "all")]
    pub select: String,

    /// Existing disk image path, passed to prepare commands as DISK_IMG.
    #[arg(long, default_value = "disk.img")]
    pub disk_img: PathBuf,

    /// Repository root, passed to prepare commands as REPO_ROOT.
    #[arg(long, default_value = ".")]
    pub repo_root: PathBuf,

    /// Tool-managed build directory, passed to prepare commands as UAPP_BUILD_DIR.
    #[arg(long, default_value = "target/uapps")]
    pub build_dir: PathBuf,
}

#[derive(Debug, Parser)]
pub struct InstallCommand {
    /// Directory containing uapp child directories.
    #[arg(long, default_value = "uapps")]
    pub uapps_dir: PathBuf,

    /// Existing disk image to mutate with debugfs.
    #[arg(long, default_value = "disk.img")]
    pub disk_img: PathBuf,

    /// Comma-separated uapp selection, or "all".
    #[arg(long, default_value = "all")]
    pub select: String,

    /// Generate commands and run prepare steps without mutating disk.img.
    #[arg(long)]
    pub dry_run: bool,

    /// Guest path for the generated autostart script.
    #[arg(long, default_value = "/etc/profile.d/99-autostart.sh")]
    pub autostart_target: String,

    /// Repository root, passed to prepare commands as REPO_ROOT.
    #[arg(long, default_value = ".")]
    pub repo_root: PathBuf,

    /// Tool-managed build directory, passed to prepare commands as UAPP_BUILD_DIR.
    #[arg(long, default_value = "target/uapps")]
    pub build_dir: PathBuf,

    /// Kernel architecture, passed to prepare commands as K_ARCH.
    #[arg(long, default_value = "")]
    pub arch: String,

    /// Rust target, passed to prepare commands as K_TARGET.
    #[arg(long, default_value = "")]
    pub target: String,

    /// Platform name, passed to prepare commands as K_PLAT_NAME.
    #[arg(long, default_value = "")]
    pub plat_name: String,

    /// Cross-compile prefix, passed to prepare commands as CROSS_COMPILE.
    #[arg(long, default_value = "")]
    pub cross_compile: String,
}
