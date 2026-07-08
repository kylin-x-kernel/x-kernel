// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    path::PathBuf,
};

use serde::Serialize;
use serde_json::Value;

use crate::{
    config::ConfigEngine,
    config::ConfigReader,
    error::{KconfigError, Result},
};

/// Build options passed via CLI flags.
pub struct BuildOpts {
    pub unittest: bool,
    pub ld_script: Option<String>,
    pub dwarf: bool,
}

impl BuildOpts {
    pub fn from_env() -> Self {
        Self {
            unittest: std::env::var("XKERNEL_UNITTEST").as_deref() == Ok("y"),
            ld_script: std::env::var("XKERNEL_LD_SCRIPT").ok(),
            dwarf: std::env::var("XKERNEL_DWARF").as_deref() == Ok("y"),
        }
    }
}

/// Generate `.cargo/.xconfig.toml` from .config and build options.
///
/// Unlike `gen-const`, this should run before every build because it depends
/// on runtime options (unittest, dwarf, ld-script) that may change between
/// invocations even when `.config` is unchanged.
pub fn gen_cargo_command(
    config: PathBuf,
    unittest: bool,
    ld_script: Option<String>,
    dwarf: bool,
) -> Result<()> {
    let config_map = load_effective_build_config(&config)?;
    let opts = BuildOpts {
        unittest,
        ld_script,
        dwarf,
    };
    generate_rust_analyzer_and_cargo_config(&config_map, &opts)?;
    Ok(())
}

fn load_effective_build_config(config: &Path) -> Result<HashMap<String, String>> {
    let raw = ConfigReader::read(config)?;
    let kconfig = Path::new("Kconfig");
    if !kconfig.exists() {
        return Ok(raw);
    }

    let mut engine = ConfigEngine::from_kconfig(kconfig, Path::new("."))?;
    engine.load_config(config)?;
    engine.refresh_prompt_state();

    let mut effective = HashMap::new();
    for (name, symbol) in engine.symbols().all_symbols() {
        if let Some(value) = &symbol.value {
            effective.insert(name.clone(), value.clone());
        }
    }

    Ok(effective)
}

/// Write `content` to `path` only if it differs from the current file content.
fn write_if_changed(path: &std::path::Path, content: &str) -> std::io::Result<bool> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        if existing == content {
            return Ok(false);
        }
    }
    std::fs::write(path, content)?;
    Ok(true)
}

/// Update `.vscode/settings.json` with rust-analyzer configuration derived
/// from `.config` values, then generate `.cargo/.xconfig.toml`.
///
/// Only `rust-analyzer.*` keys are modified; all other settings are preserved.
pub fn generate_rust_analyzer_and_cargo_config(
    config: &HashMap<String, String>,
    opts: &BuildOpts,
) -> Result<()> {
    generate_rust_analyzer_config(config)?;
    generate_cargo_config(config, opts)
}

fn resolve_arch(config: &HashMap<String, String>) -> &'static str {
    if config.get("ARCH_AARCH64") == Some(&"y".to_string()) {
        "aarch64"
    } else if config.get("ARCH_RISCV64") == Some(&"y".to_string()) {
        "riscv64"
    } else if config.get("ARCH_X86_64") == Some(&"y".to_string()) {
        "x86_64"
    } else if config.get("ARCH_LOONGARCH64") == Some(&"y".to_string()) {
        "loongarch64"
    } else {
        "unknown"
    }
}

fn resolve_target(arch: &str) -> Option<&'static str> {
    match arch {
        "x86_64" => Some("x86_64-unknown-none"),
        "aarch64" => Some("aarch64-unknown-none-softfloat"),
        "riscv64" => Some("riscv64gc-unknown-none-elf"),
        "loongarch64" => Some("loongarch64-unknown-none-softfloat"),
        _ => None,
    }
}

fn resolve_plat_name(config: &HashMap<String, String>) -> &'static str {
    if config.get("PLATFORM_AARCH64_QEMU_VIRT") == Some(&"y".to_string()) {
        "aarch64-qemu-virt"
    } else if config.get("PLATFORM_AARCH64_RASPI") == Some(&"y".to_string()) {
        "aarch64-raspi"
    } else if config.get("PLATFORM_RISCV64_QEMU_VIRT") == Some(&"y".to_string()) {
        "riscv64-qemu-virt"
    } else if config.get("PLATFORM_X86_64_QEMU_VIRT") == Some(&"y".to_string()) {
        "x86_64-qemu-virt"
    } else if config.get("PLATFORM_LOONGARCH64_QEMU_VIRT") == Some(&"y".to_string()) {
        "loongarch64-qemu-virt"
    } else {
        "unknown"
    }
}

fn generate_rust_analyzer_config(config: &HashMap<String, String>) -> Result<()> {
    let arch = resolve_arch(config);
    let Some(target) = resolve_target(arch) else {
        return Ok(());
    };

    let features = extract_kfeat_features(config);
    let feature_values: Vec<String> = features
        .iter()
        .map(|f| format!("kfeat/{}", f))
        .collect();

    let managed_keys = [
        "rust-analyzer.cargo.target",
        "rust-analyzer.cargo.noDefaultFeatures",
        "rust-analyzer.cargo.cfgs",
        "rust-analyzer.cargo.features",
        "rust-analyzer.check.targets",
        "rust-analyzer.check.allTargets",
        "rust-analyzer.cfg.setTest",
    ];

    let vscode_dir = std::path::Path::new(".vscode");
    let settings_path = vscode_dir.join("settings.json");
    let mut settings: serde_json::Map<String, Value> = if settings_path.exists() {
        let content = std::fs::read_to_string(&settings_path).map_err(KconfigError::Io)?;
        serde_json::from_str::<Value>(&content)
            .unwrap_or(Value::Object(serde_json::Map::new()))
            .as_object()
            .cloned()
            .unwrap_or_default()
    } else {
        serde_json::Map::new()
    };

    for key in &managed_keys {
        settings.remove(*key);
    }

    settings.insert(
        "rust-analyzer.cargo.target".into(),
        Value::String(target.into()),
    );
    settings.insert(
        "rust-analyzer.cargo.features".into(),
        Value::Array(
            feature_values
                .iter()
                .map(|f| Value::String(f.clone()))
                .collect(),
        ),
    );
    settings.insert("rust-analyzer.cfg.setTest".into(), Value::Bool(false));
    settings.insert(
        "rust-analyzer.check.allTargets".into(),
        Value::Bool(false),
    );

    if !vscode_dir.exists() {
        std::fs::create_dir(vscode_dir).map_err(KconfigError::Io)?;
    }
    let output = serde_json::to_string_pretty(&Value::Object(settings))
        .map_err(|e| KconfigError::Config(e.to_string()))?;
    let output_with_newline = format!("{}\n", output);
    let changed =
        write_if_changed(&settings_path, &output_with_newline).map_err(KconfigError::Io)?;
    if changed {
        println!("✅ Updated .vscode/settings.json");
    }

    let ra_toml_path = std::path::Path::new("rust-analyzer.toml");
    if ra_toml_path.exists() {
        std::fs::remove_file(ra_toml_path).map_err(KconfigError::Io)?;
        println!("🗑️  Removed stale rust-analyzer.toml");
    }

    Ok(())
}

fn generate_cargo_config(config: &HashMap<String, String>, opts: &BuildOpts) -> Result<()> {
    let arch = resolve_arch(config);
    let Some(target) = resolve_target(arch) else {
        return Ok(());
    };
    let plat_name = resolve_plat_name(config);

    let features = extract_kfeat_features(config);
    let feature_values: Vec<String> = features
        .iter()
        .map(|f| format!("kfeat/{}", f))
        .collect();

    let dot_cargo_dir = std::path::Path::new(".cargo");
    if !dot_cargo_dir.exists() {
        std::fs::create_dir(dot_cargo_dir).map_err(|e| KconfigError::Io(e))?;
    }

    let mut all_plat_names: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir("platforms") {
        for entry in entries.flatten() {
            let path = entry.path();
            let has_defconfig = path.join("defconfig").exists();
            let has_plat_cfg = path.join("src/lib.rs").exists()
                && std::fs::read_to_string(path.join("src/lib.rs"))
                    .map(|c| c.contains("k_plat_name"))
                    .unwrap_or(false);
            if has_defconfig || has_plat_cfg {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    all_plat_names.push(name.to_string());
                }
            }
        }
    }
    all_plat_names.sort();

    let plat_values: String = all_plat_names
        .iter()
        .map(|p| format!("\"{}\"", p))
        .collect::<Vec<_>>()
        .join(", ");

    let mut rustflags: Vec<String> = vec!["--check-cfg".into(), "cfg(unittest)".into()];
    if opts.unittest {
        rustflags.push("--cfg".into());
        rustflags.push("unittest".into());
    }
    if !plat_values.is_empty() {
        rustflags.push("--check-cfg".into());
        rustflags.push(format!("cfg(k_plat_name, values({}))", plat_values));
        rustflags.push("--cfg".into());
        rustflags.push(format!("k_plat_name=\"{}\"", plat_name));
    }

    let mut target_rustflags = rustflags.clone();

    if let Some(ld_script) = &opts.ld_script {
        target_rustflags.push("-C".into());
        target_rustflags.push(format!("link-arg=-T{}", ld_script));
        target_rustflags.push("-C".into());
        target_rustflags.push("link-arg=-no-pie".into());
        target_rustflags.push("-C".into());
        target_rustflags.push("link-arg=-znostart-stop-gc".into());
    }

    if opts.dwarf {
        target_rustflags.push("-C".into());
        target_rustflags.push("force-frame-pointers".into());
        target_rustflags.push("-C".into());
        target_rustflags.push("debuginfo=2".into());
        target_rustflags.push("-C".into());
        target_rustflags.push("strip=none".into());
    }

    if opts.unittest {
        target_rustflags.push("-C".into());
        target_rustflags.push("instrument-coverage".into());
        target_rustflags.push("-Z".into());
        target_rustflags.push("no-profiler-runtime".into());
        // One codegen unit per crate so that any live reference to a crate
        // pulls its whole compilation unit — including `#[def_test]` descriptors
        // in `.unittest` — into the link, where the linker-script `KEEP` retains
        // them. Without this, test-only codegen units of large crates (e.g.
        // ksyscall, which is otherwise unreferenced in the unittest image) get
        // dropped by `--gc-sections`.
        target_rustflags.push("-C".into());
        target_rustflags.push("codegen-units=1".into());
    }

    let mut envs = BTreeMap::new();
    envs.insert("K_PLAT_NAME".to_string(), plat_name.into());
    envs.insert(
        "RUST_TARGET_PATH".to_string(),
        format!("platforms/{}", plat_name),
    );

    #[derive(Serialize)]
    struct CargoConfig {
        build: BuildTarget,
        #[serde(rename = "target")]
        targets: BTreeMap<String, BuildTarget>,
        env: BTreeMap<String, String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        #[serde(rename = "features")]
        feature_values: Vec<String>,
    }

    #[derive(Serialize)]
    struct BuildTarget {
        rustflags: Vec<String>,
    }

    let mut targets = BTreeMap::new();
    targets.insert(
        target.to_string(),
        BuildTarget {
            rustflags: target_rustflags,
        },
    );

    let config_obj = CargoConfig {
        build: BuildTarget { rustflags },
        targets,
        env: envs,
        feature_values,
    };

    let generated_toml =
        toml::to_string_pretty(&config_obj).map_err(|e| KconfigError::Config(e.to_string()))?;
    let cargo_config_toml = format!(
        "# Automatically generated file; DO NOT EDIT.\n\
         # Derived from .config by xtask xconfig gen-cargo.\n\
         \n\
         {}\n",
        generated_toml
    );
    let xconfig_path = dot_cargo_dir.join(".xconfig.toml");
    let changed = write_if_changed(&xconfig_path, &cargo_config_toml).map_err(KconfigError::Io)?;
    if changed {
        println!("✅ Generated .cargo/.xconfig.toml");
    }

    Ok(())
}

/// Extract cargo feature names from .config entries, mirroring
/// `scripts/make/kfeat_features.sh`.
///
/// Maps `KFEAT_<NAME>=y` to feature `<name>` (lowercase) and
/// `PLATFORM_<NAME>=y` to feature `platform_<name>` (lowercase),
/// skipping build-time-only keys.
fn extract_kfeat_features(config: &HashMap<String, String>) -> Vec<String> {
    const SKIP_KEYS: &[&str] = &["KFEAT_FS", "KFEAT_VIRTIO_BUS_PCI", "KFEAT_VIRTIO_BUS_MMIO"];

    let mut features: Vec<String> = Vec::new();

    for (key, value) in config {
        if value != "y" {
            continue;
        }

        if let Some(name) = key.strip_prefix("KFEAT_") {
            if name.is_empty() {
                continue;
            }
            if !name
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            {
                continue;
            }
            if SKIP_KEYS.contains(&key.as_str()) {
                continue;
            }
            features.push(name.to_lowercase());
        } else if let Some(name) = key.strip_prefix("PLATFORM_") {
            if name.is_empty() {
                continue;
            }
            if !name
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            {
                continue;
            }
            features.push(format!("platform_{}", name.to_lowercase()));
        }
    }

    features.sort();
    features.dedup();
    features
}
