// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{fs, net::Ipv4Addr, path::PathBuf};

use serde::Deserialize;
use xconfig::build_config::{
    KernelBuildFiles, ResolvedKernelConfig, prepare_kernel_config, resolve_kernel_config,
};

use crate::{
    cli::{BuildArgs, ConfigArgs, ConfigField},
    error::{Error, IoResultExt, Result},
};

pub(crate) struct BuildContext {
    pub(crate) workspace_root: PathBuf,
    pub(crate) app_manifest: PathBuf,
    pub(crate) package_name: String,
    pub(crate) target_dir: PathBuf,
    pub(crate) kbuild_dir: PathBuf,
    pub(crate) linker_script: PathBuf,
    pub(crate) cargo_elf: PathBuf,
    pub(crate) bundle_dir: PathBuf,
    pub(crate) bundle_elf: PathBuf,
    pub(crate) bundle_debug_elf: PathBuf,
    pub(crate) bundle_bin: PathBuf,
    pub(crate) bundle_linuxboot: PathBuf,
    pub(crate) bundle_uefi: PathBuf,
    pub(crate) config: ResolvedKernelConfig,
    pub(crate) guest_ip: Ipv4Addr,
    pub(crate) gateway: Ipv4Addr,
    pub(crate) unittest: bool,
    pub(crate) unittest_crate: Option<String>,
    pub(crate) dry_run: bool,
    pub(crate) verbosity: u8,
}

impl BuildContext {
    pub(crate) fn create(args: &BuildArgs) -> Result<Self> {
        let workspace_root = workspace_root()?;

        let config_path = workspace_root.join(&args.workspace.config);
        ensure_config_exists(&config_path)?;

        let kconfig_path = workspace_root.join("Kconfig");
        let config = if args.workspace.dry_run {
            resolve_kernel_config(&config_path, &kconfig_path, &workspace_root)?
        } else {
            prepare_kernel_config(&config_path, &kconfig_path, &workspace_root)?
        };

        let target_dir = workspace_root.join(&args.workspace.target_dir);
        let app_manifest = workspace_root.join(&args.app).join("Cargo.toml");
        let package_name = read_package_name(&app_manifest)?;
        let profile = config.profile().as_str();
        let target = config.target();
        let platform = config.platform();
        let kbuild_dir = target_dir.join("kbuild").join(platform);
        let linker_script = target_dir
            .join(target)
            .join(profile)
            .join(format!("linker_{platform}.lds"));
        let cargo_elf = target_dir.join(target).join(profile).join(&package_name);
        let bundle_dir = target_dir.join("xkmake").join(platform).join(profile);
        let bundle_elf = bundle_dir.join("kernel.elf");
        let bundle_debug_elf = bundle_dir.join("kernel.debug.elf");
        let bundle_bin = bundle_dir.join("kernel.bin");
        let bundle_linuxboot = bundle_dir.join("kernel.bzimg");
        let bundle_uefi = bundle_dir.join("kernel.uefi.img");

        Ok(Self {
            workspace_root,
            app_manifest,
            package_name,
            target_dir,
            kbuild_dir,
            linker_script,
            cargo_elf,
            bundle_dir,
            bundle_elf,
            bundle_debug_elf,
            bundle_bin,
            bundle_linuxboot,
            bundle_uefi,
            config,
            guest_ip: args.guest_ip,
            gateway: args.gateway,
            unittest: args.unittest,
            unittest_crate: args.unittest_crate.clone(),
            dry_run: args.workspace.dry_run,
            verbosity: args.workspace.verbosity,
        })
    }

    pub(crate) fn build_files(&self) -> KernelBuildFiles {
        KernelBuildFiles {
            rust_const_dir: self.kbuild_dir.clone(),
            linker_script: self.linker_script.clone(),
            unittest: self.unittest,
        }
    }
}

pub(crate) fn print_config(args: &ConfigArgs) -> Result<()> {
    let workspace_root = workspace_root()?;
    let config_path = workspace_root.join(&args.config);
    ensure_config_exists(&config_path)?;
    let config = resolve_kernel_config(
        &config_path,
        workspace_root.join("Kconfig"),
        &workspace_root,
    )?;
    let bundle_dir = workspace_root
        .join(&args.target_dir)
        .join("xkmake")
        .join(config.platform())
        .join(config.profile().as_str());

    match args.field {
        ConfigField::Arch => println!("{}", config.arch().as_str()),
        ConfigField::Target => println!("{}", config.target()),
        ConfigField::Platform => println!("{}", config.platform()),
        ConfigField::Profile => println!("{}", config.profile().as_str()),
        ConfigField::NrCpus => println!("{}", config.nr_cpus()),
        ConfigField::CrossCompile => println!("{}", config.arch().cross_compile_prefix()),
        ConfigField::BundleDir => println!("{}", bundle_dir.display()),
        ConfigField::BundleElf => println!("{}", bundle_dir.join("kernel.elf").display()),
        ConfigField::BundleBin => println!("{}", bundle_dir.join("kernel.bin").display()),
        ConfigField::BundleLinuxboot => {
            println!("{}", bundle_dir.join("kernel.bzimg").display())
        }
        ConfigField::BundleUefi => {
            println!("{}", bundle_dir.join("kernel.uefi.img").display())
        }
    }
    Ok(())
}

pub(crate) fn workspace_root() -> Result<PathBuf> {
    let workspace_root = std::env::current_dir()
        .with_path("current directory")?
        .canonicalize()
        .with_path("current directory")?;
    ensure_workspace_root(&workspace_root)?;
    Ok(workspace_root)
}

pub(crate) fn ensure_config_exists(config_path: &std::path::Path) -> Result<()> {
    if !config_path.is_file() {
        return Err(Error::Message(format!(
            "kernel configuration not found: {}; copy a platform defconfig to .config first",
            config_path.display()
        )));
    }
    Ok(())
}

fn ensure_workspace_root(workspace_root: &std::path::Path) -> Result<()> {
    for required in ["Cargo.toml", "Kconfig", "xtask/Cargo.toml"] {
        if !workspace_root.join(required).is_file() {
            return Err(Error::Message(format!(
                "{} is not an X-Kernel workspace root: missing {required}",
                workspace_root.display()
            )));
        }
    }
    Ok(())
}

fn read_package_name(manifest_path: &std::path::Path) -> Result<String> {
    #[derive(Deserialize)]
    struct Manifest {
        package: Package,
    }

    #[derive(Deserialize)]
    struct Package {
        name: String,
    }

    let content = fs::read_to_string(manifest_path).with_path(manifest_path)?;
    let manifest = toml::from_str::<Manifest>(&content)
        .map_err(|error| Error::Message(format!("invalid {}: {error}", manifest_path.display())))?;
    Ok(manifest.package.name)
}
