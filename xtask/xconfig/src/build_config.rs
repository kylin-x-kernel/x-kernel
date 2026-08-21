// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Stable build-facing access to an evaluated kernel configuration.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
};

use crate::{
    cli::gen_cargo::{BuildOpts, generate_rust_analyzer_and_cargo_config},
    config::{ConfigEngine, ConfigGenerator},
    error::{KconfigError, Result},
    kconfig::SymbolType,
};

/// A supported X-Kernel target architecture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KernelArch {
    /// 64-bit Arm.
    Aarch64,
    /// 64-bit RISC-V.
    Riscv64,
    /// 64-bit x86.
    X86_64,
    /// 64-bit LoongArch.
    LoongArch64,
}

impl KernelArch {
    /// Returns the architecture name used by X-Kernel tools.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Aarch64 => "aarch64",
            Self::Riscv64 => "riscv64",
            Self::X86_64 => "x86_64",
            Self::LoongArch64 => "loongarch64",
        }
    }

    /// Returns the Rust target triple for this architecture.
    pub const fn target(self) -> &'static str {
        match self {
            Self::Aarch64 => "aarch64-unknown-none-softfloat",
            Self::Riscv64 => "riscv64gc-unknown-none-elf",
            Self::X86_64 => "x86_64-unknown-none",
            Self::LoongArch64 => "loongarch64-unknown-none-softfloat",
        }
    }

    /// Returns the conventional musl cross-toolchain prefix.
    pub const fn cross_compile_prefix(self) -> &'static str {
        match self {
            Self::Aarch64 => "aarch64-linux-musl-",
            Self::Riscv64 => "riscv64-linux-musl-",
            Self::X86_64 => "x86_64-linux-musl-",
            Self::LoongArch64 => "loongarch64-linux-musl-",
        }
    }
}

/// The Cargo profile selected by Kconfig.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuildProfile {
    /// Cargo's development profile.
    Debug,
    /// Cargo's optimized release profile.
    Release,
}

impl BuildProfile {
    /// Returns the Cargo output directory name for the profile.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Release => "release",
        }
    }
}

/// The virtio transport selected for the compiled platform.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VirtioBus {
    /// Virtio MMIO transport.
    Mmio,
    /// Virtio PCI transport.
    Pci,
}

/// Fully evaluated kernel configuration required by build orchestration.
#[derive(Clone, Debug)]
pub struct ResolvedKernelConfig {
    arch: KernelArch,
    platform: String,
    profile: BuildProfile,
    nr_cpus: usize,
    virtio_bus: Option<VirtioBus>,
    cargo_features: Vec<String>,
    values: BTreeMap<String, String>,
    symbol_types: HashMap<String, SymbolType>,
}

/// Output paths and build-mode inputs applied to a resolved configuration.
///
/// This structure does not identify `.config` or Kconfig inputs. Generation
/// consumes the authoritative values and symbol types already captured in
/// [`ResolvedKernelConfig`].
#[derive(Clone, Debug)]
pub struct KernelBuildFiles {
    /// Directory that receives the generated Rust constants.
    pub rust_const_dir: PathBuf,
    /// Linker script selected for the resolved platform and profile.
    pub linker_script: PathBuf,
    /// Whether generated Cargo flags enable kernel unit-test mode.
    pub unittest: bool,
}

impl ResolvedKernelConfig {
    /// Returns the configured architecture.
    pub const fn arch(&self) -> KernelArch {
        self.arch
    }

    /// Returns the configured platform name.
    pub fn platform(&self) -> &str {
        &self.platform
    }

    /// The target machine/board name (e.g. "qemu", "rk3588") from the
    /// MACHINE Kconfig symbol. Combined with [`arch`](Self::arch) this forms
    /// the `arch-machine` stem used for root artifact naming.
    pub fn machine(&self) -> &str {
        self.values
            .get("MACHINE")
            .map(String::as_str)
            .unwrap_or("unknown")
    }

    /// Returns the Rust target triple.
    pub const fn target(&self) -> &'static str {
        self.arch.target()
    }

    /// Returns the configured Cargo profile.
    pub const fn profile(&self) -> BuildProfile {
        self.profile
    }

    /// Returns the compile-time maximum CPU count.
    pub const fn nr_cpus(&self) -> usize {
        self.nr_cpus
    }

    /// Returns the configured virtio transport, if any.
    pub const fn virtio_bus(&self) -> Option<VirtioBus> {
        self.virtio_bus
    }

    /// Returns the Cargo features derived from Kconfig selections.
    pub fn cargo_features(&self) -> &[String] {
        &self.cargo_features
    }

    /// Returns the effective value of a Kconfig symbol.
    pub fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// Returns whether a boolean or tristate symbol is enabled.
    pub fn is_enabled(&self, name: &str) -> bool {
        matches!(self.value(name), Some("y" | "m"))
    }

    /// Returns all effective Kconfig values in deterministic order.
    pub const fn values(&self) -> &BTreeMap<String, String> {
        &self.values
    }
}

/// Expand and validate a kernel configuration, write its standard artifacts,
/// and return the resolved build-facing view.
///
/// # Errors
///
/// Returns an error when Kconfig cannot be parsed or evaluated, the input
/// configuration is invalid, or a generated artifact cannot be written.
pub fn prepare_kernel_config(
    config_path: impl AsRef<Path>,
    kconfig_path: impl AsRef<Path>,
    source_tree: impl AsRef<Path>,
) -> Result<ResolvedKernelConfig> {
    let config_path = config_path.as_ref();
    let engine = load_engine(config_path, kconfig_path, source_tree)?;
    engine.write_artifacts(config_path)?;

    resolve_engine(engine)
}

/// Evaluate a kernel configuration without writing generated artifacts.
///
/// # Errors
///
/// Returns an error when Kconfig cannot be parsed or evaluated or the input
/// configuration does not resolve to a supported build target.
pub fn resolve_kernel_config(
    config_path: impl AsRef<Path>,
    kconfig_path: impl AsRef<Path>,
    source_tree: impl AsRef<Path>,
) -> Result<ResolvedKernelConfig> {
    resolve_engine(load_engine(
        config_path.as_ref(),
        kconfig_path,
        source_tree,
    )?)
}

/// Generate all Rust and Cargo configuration consumed by a kernel build.
///
/// Callers must resolve the configuration first so architecture-dependent paths,
/// especially the linker script, are finalized before this function is called.
/// This function neither reads `.config` nor parses Kconfig again.
///
/// # Errors
///
/// Returns an error when generated Rust constants, rust-analyzer settings, or
/// Cargo configuration files cannot be written.
pub fn generate_kernel_build_files(
    config: &ResolvedKernelConfig,
    files: &KernelBuildFiles,
) -> Result<()> {
    let values = config
        .values
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect::<HashMap<_, _>>();

    println!("📝 Generating Rust const definitions from resolved Kconfig...");
    println!("Output: {}", files.rust_const_dir.display());
    ConfigGenerator::generate_rust_consts(&files.rust_const_dir, &values, &config.symbol_types)?;
    println!("✅ Generated config.rs successfully");

    let build_options = BuildOpts {
        unittest: files.unittest,
        ld_script: Some(files.linker_script.to_string_lossy().into_owned()),
    };
    generate_rust_analyzer_and_cargo_config(&values, &build_options)
}

fn load_engine(
    config_path: &Path,
    kconfig_path: impl AsRef<Path>,
    source_tree: impl AsRef<Path>,
) -> Result<ConfigEngine> {
    let mut engine = ConfigEngine::from_kconfig(kconfig_path, source_tree)?;
    engine.load_config(config_path)?;
    engine.refresh_prompt_state();
    engine.prune_inactive_symbols();
    Ok(engine)
}

fn resolve_engine(engine: ConfigEngine) -> Result<ResolvedKernelConfig> {
    let mut values = BTreeMap::new();
    let mut symbol_types = HashMap::new();
    for (name, symbol) in engine.symbols().all_symbols() {
        symbol_types.insert(name.clone(), symbol.symbol_type.clone());
        if let Some(value) = &symbol.value {
            values.insert(name.clone(), value.clone());
        }
    }

    let arch = match required_value(&values, "ARCH")? {
        "aarch64" => KernelArch::Aarch64,
        "riscv64" => KernelArch::Riscv64,
        "x86_64" => KernelArch::X86_64,
        "loongarch64" => KernelArch::LoongArch64,
        value => return Err(invalid_value("ARCH", value)),
    };
    let platform = match arch {
        KernelArch::Aarch64 => "kplat-aarch64",
        KernelArch::Riscv64 => "kplat-riscv64",
        KernelArch::X86_64 => "kplat-x86_64",
        KernelArch::LoongArch64 => "kplat-loongarch64",
    }
    .to_string();
    let profile = if enabled(&values, "BUILD_TYPE_DEBUG") {
        BuildProfile::Debug
    } else if enabled(&values, "BUILD_TYPE_RELEASE") {
        BuildProfile::Release
    } else {
        return Err(KconfigError::Config(
            "neither BUILD_TYPE_DEBUG nor BUILD_TYPE_RELEASE is selected".to_string(),
        ));
    };
    let nr_cpus_value = required_value(&values, "NR_CPUS")?;
    let nr_cpus = nr_cpus_value
        .parse::<usize>()
        .map_err(|_| invalid_value("NR_CPUS", nr_cpus_value))?;
    let virtio_bus = match (
        enabled(&values, "KFEAT_VIRTIO_BUS_MMIO"),
        enabled(&values, "KFEAT_VIRTIO_BUS_PCI"),
    ) {
        (false, false) => None,
        (true, false) => Some(VirtioBus::Mmio),
        (false, true) => Some(VirtioBus::Pci),
        (true, true) => {
            return Err(KconfigError::Config(
                "both KFEAT_VIRTIO_BUS_MMIO and KFEAT_VIRTIO_BUS_PCI are enabled".to_string(),
            ));
        }
    };

    let cargo_features = extract_cargo_features(&values);

    Ok(ResolvedKernelConfig {
        arch,
        platform,
        profile,
        nr_cpus,
        virtio_bus,
        cargo_features,
        values,
        symbol_types,
    })
}

fn required_value<'a>(values: &'a BTreeMap<String, String>, name: &str) -> Result<&'a str> {
    values
        .get(name)
        .map(String::as_str)
        .ok_or_else(|| KconfigError::Config(format!("required symbol {name} has no value")))
}

fn invalid_value(name: &str, value: &str) -> KconfigError {
    KconfigError::Config(format!("invalid value for {name}: {value}"))
}

fn enabled(values: &BTreeMap<String, String>, name: &str) -> bool {
    matches!(values.get(name).map(String::as_str), Some("y" | "m"))
}

fn extract_cargo_features(values: &BTreeMap<String, String>) -> Vec<String> {
    const SKIP_KEYS: &[&str] = &["KFEAT_FS", "KFEAT_VIRTIO_BUS_PCI", "KFEAT_VIRTIO_BUS_MMIO"];

    let mut features = values
        .iter()
        .filter(|(_, value)| value.as_str() == "y")
        .filter_map(|(name, _)| {
            if SKIP_KEYS.contains(&name.as_str()) {
                return None;
            }
            name.strip_prefix("KFEAT_")
                .map(|feature| feature.to_ascii_lowercase())
                .or_else(|| {
                    name.strip_prefix("PLATFORM_")
                        .map(|platform| format!("platform_{}", platform.to_ascii_lowercase()))
                })
                .or_else(|| {
                    // main's MACHINE-based config: MACHINE_<ARCH>_<BOARD>=y
                    // (e.g. MACHINE_AARCH64_QEMU) maps to the HAL-crate
                    // feature platform_kplat_<arch>.  X86_64 contains an
                    // underscore, so match by explicit arch prefix.
                    const ARCHES: [(&str, &str); 4] = [
                        ("AARCH64_", "aarch64"),
                        ("RISCV64_", "riscv64"),
                        ("X86_64_", "x86_64"),
                        ("LOONGARCH64_", "loongarch64"),
                    ];
                    name.strip_prefix("MACHINE_").and_then(|machine| {
                        ARCHES
                            .iter()
                            .find(|(prefix, _)| machine.starts_with(prefix))
                            .map(|(_, arch)| format!("platform_kplat_{}", arch))
                    })
                })
        })
        .collect::<Vec<_>>();
    features.sort();
    features.dedup();
    features
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_features_are_sorted_and_skip_transport_markers() {
        let values = BTreeMap::from([
            ("KFEAT_NET".to_string(), "y".to_string()),
            ("KFEAT_FS".to_string(), "y".to_string()),
            ("KFEAT_VIRTIO_BUS_PCI".to_string(), "y".to_string()),
            ("PLATFORM_AARCH64_QEMU_VIRT".to_string(), "y".to_string()),
        ]);

        assert_eq!(
            extract_cargo_features(&values),
            ["net", "platform_aarch64_qemu_virt"]
        );
    }
}
