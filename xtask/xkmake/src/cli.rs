// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{net::Ipv4Addr, path::PathBuf};

use clap::{Args, Parser, Subcommand, ValueEnum};

/// Build and run X-Kernel.
#[derive(Debug, Parser)]
#[command(name = "xkmake")]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Build the configured kernel and create a bundle.
    Build(BuildArgs),
    /// Check the configured kernel with Clippy.
    Clippy(BuildArgs),
    /// Generate workspace rustdoc for the configured kernel features.
    Doc(DocArgs),
    /// Build the configured kernel and run its bundle with QEMU.
    Run(RunArgs),
    /// Print one resolved configuration or artifact value.
    Config(ConfigArgs),
    /// Run repository hygiene checks.
    Hygiene(HygieneArgs),
}

#[derive(Debug, Args)]
pub(crate) struct HygieneArgs {
    #[command(subcommand)]
    pub(crate) command: HygieneCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum HygieneCommand {
    /// Install the pinned repository hygiene tools.
    InstallTools,
    /// Check Cargo manifests for unused dependencies.
    Deps(HygieneFixArgs),
    /// Check Rust source license headers.
    Header(HygieneFixArgs),
}

#[derive(Debug, Args)]
pub(crate) struct HygieneFixArgs {
    /// Apply the available automatic fixes.
    #[arg(long)]
    pub(crate) fix: bool,
}

#[derive(Debug, Args)]
pub(crate) struct ConfigArgs {
    /// Value to print.
    #[arg(value_enum)]
    pub(crate) field: ConfigField,

    /// Path to the expanded or seed kernel configuration.
    #[arg(long, default_value = ".config")]
    pub(crate) config: PathBuf,

    /// Cargo target directory used to derive bundle paths.
    #[arg(long, default_value = "target")]
    pub(crate) target_dir: PathBuf,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub(crate) enum ConfigField {
    Arch,
    Target,
    Platform,
    Profile,
    NrCpus,
    CrossCompile,
    BundleDir,
    BundleElf,
    BundleBin,
    BundleLinuxboot,
    BundleUefi,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum X86BootMode {
    Linuxboot,
    Uefi,
}

#[derive(Clone, Debug, Args)]
pub(crate) struct WorkspaceArgs {
    /// Path to the expanded or seed kernel configuration.
    #[arg(long, default_value = ".config")]
    pub(crate) config: PathBuf,

    /// Cargo target directory.
    #[arg(long, default_value = "target")]
    pub(crate) target_dir: PathBuf,

    /// Print commands without executing them.
    #[arg(long)]
    pub(crate) dry_run: bool,

    /// Increase command verbosity.
    #[arg(short = 'v', action = clap::ArgAction::Count)]
    pub(crate) verbosity: u8,
}

#[derive(Clone, Debug, Args)]
pub(crate) struct BuildArgs {
    #[command(flatten)]
    pub(crate) workspace: WorkspaceArgs,

    /// Kernel application crate.
    #[arg(long, default_value = "entry")]
    pub(crate) app: PathBuf,

    /// Static guest IPv4 address compiled into the current network stack.
    #[arg(long, default_value = "10.0.2.15")]
    pub(crate) guest_ip: Ipv4Addr,

    /// Static guest gateway compiled into the current network stack.
    #[arg(long, default_value = "10.0.2.2")]
    pub(crate) gateway: Ipv4Addr,

    /// Build a kernel unit-test image.
    #[arg(long)]
    pub(crate) unittest: bool,

    /// Restrict kernel unit tests to a crate or comma-separated crate list.
    #[arg(long, requires = "unittest")]
    pub(crate) unittest_crate: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct DocArgs {
    #[command(flatten)]
    pub(crate) workspace: WorkspaceArgs,

    /// Deny missing rustdoc on public APIs.
    #[arg(long)]
    pub(crate) check_missing: bool,

    /// Open the generated documentation in a browser.
    #[arg(long)]
    pub(crate) open: bool,

    /// Additional arguments passed to `cargo doc`.
    #[arg(last = true)]
    pub(crate) cargo_args: Vec<String>,
}

#[derive(Debug, Args)]
pub(crate) struct RunArgs {
    #[command(flatten)]
    pub(crate) build: BuildArgs,

    /// Run an existing compatible bundle without invoking Cargo.
    #[arg(long)]
    pub(crate) no_build: bool,

    /// x86_64 boot protocol. Defaults to LinuxBoot on x86_64.
    #[arg(long, value_enum)]
    pub(crate) boot: Option<X86BootMode>,

    /// OVMF executable firmware used by x86_64 UEFI boot.
    #[arg(long)]
    pub(crate) ovmf_code: Option<PathBuf>,

    /// OVMF variable-store template copied for each x86_64 UEFI run.
    #[arg(long)]
    pub(crate) ovmf_vars_template: Option<PathBuf>,

    /// Guest memory size.
    #[arg(long, default_value = "1g")]
    pub(crate) memory: String,

    /// Number of virtual CPUs. Defaults to the configured NR_CPUS.
    #[arg(long)]
    pub(crate) smp: Option<usize>,

    /// Root disk image.
    #[arg(long, default_value = "disk.img")]
    pub(crate) disk_image: PathBuf,

    /// Disable the configured virtio block device.
    #[arg(long)]
    pub(crate) no_block: bool,

    /// Disable the configured virtio network device.
    #[arg(long)]
    pub(crate) no_net: bool,

    /// Disable automatic vhost-vsock attachment.
    #[arg(long)]
    pub(crate) no_vsock: bool,

    /// Disable host hardware acceleration.
    #[arg(long)]
    pub(crate) no_accel: bool,

    /// Enable graphical output.
    #[arg(long)]
    pub(crate) graphic: bool,

    /// Host port forwarded to guest TCP and UDP port 5555.
    #[arg(long, default_value_t = 5555)]
    pub(crate) hostfwd_port: u16,

    /// Guest CID for the vsock device.
    #[arg(long, default_value_t = 103)]
    pub(crate) vsock_cid: u32,

    /// Additional QEMU arguments.
    #[arg(last = true)]
    pub(crate) qemu_args: Vec<String>,
}
