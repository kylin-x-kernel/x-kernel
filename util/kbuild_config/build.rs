// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{env, fs, path::Path};

fn main() {
    // Get the path to the generated config.rs
    let out_dir = env::var("OUT_DIR").unwrap();
    let config_rs_path = Path::new(&out_dir).join("config.rs");

    // If cargo-kbuild has generated the config.rs, copy it to OUT_DIR
    // Otherwise, generate an empty one
    let workspace_root = env::var("CARGO_MANIFEST_DIR").unwrap();
    let workspace_root = Path::new(&workspace_root)
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let target_dir = env::var("TARGET_DIR")
        .unwrap_or_else(|_| workspace_root.join("target").to_string_lossy().into_owned());
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let default_plat_name = match target_arch.as_str() {
        "x86_64" => "kplat-x86_64",
        "aarch64" => "kplat-aarch64",
        "riscv64" => "kplat-riscv64",
        "loongarch64" => "kplat-loongarch64",
        _ => "kplat-aarch64",
    };
    let plat_name = env::var("K_PLAT_NAME").unwrap_or_else(|_| default_plat_name.to_string());
    let target_config_path = Path::new(&target_dir)
        .join("kbuild")
        .join(plat_name)
        .join("config.rs");

    if target_config_path.exists() {
        // Copy the generated config
        let mut config_content =
            fs::read_to_string(&target_config_path).expect("Failed to read generated config.rs");
        // x86-only Kconfig symbols are absent on other platforms but may still be
        // referenced when host-side tools (e.g. `cargo doc`) compile x86_64 code.
        if !config_content.contains("SEV_CBIT_POS") {
            config_content.push_str("\npub const SEV_CBIT_POS: u32 = 0;\n");
        }
        if !config_content.contains("ENTROPY_PHYTIUM_TRNG_PADDR") {
            config_content.push_str("\npub const ENTROPY_PHYTIUM_TRNG_PADDR: usize = 0;\n");
        }
        // Arch-/driver-gated entropy bools may be omitted from older configs or
        // when a symbol was previously `depends on` an unmet arch. Shared crates
        // reference them unconditionally, so default missing ones to false.
        for name in [
            "KFEAT_ENTROPY_ARCH_CPU",
            "KFEAT_ENTROPY_SMCCC_TRNG",
            "KFEAT_ENTROPY_TRUST_HOST",
            "KFEAT_ENTROPY_JITTER",
            "KFEAT_DRIVER_VIRTIO_RNG",
        ] {
            if !config_content.contains(name) {
                config_content.push_str(&format!("\npub const {name}: bool = false;\n"));
            }
        }
        fs::write(&config_rs_path, config_content).expect("Failed to write config.rs to OUT_DIR");
    } else {
        // Generate empty config if not available
        fs::write(
            &config_rs_path,
            "// No config.rs generated yet\npub const SEV_CBIT_POS: u32 = 0;\npub const \
             KFEAT_ENTROPY_ARCH_CPU: bool = false;\npub const KFEAT_ENTROPY_SMCCC_TRNG: bool = \
             false;\npub const KFEAT_ENTROPY_TRUST_HOST: bool = false;\npub const \
             KFEAT_ENTROPY_JITTER: bool = false;\npub const KFEAT_DRIVER_VIRTIO_RNG: bool = \
             false;\n",
        )
        .expect("Failed to write empty config.rs");
    }

    // Set environment variable for inclusion
    println!(
        "cargo:rustc-env=CONFIG_RS_PATH={}",
        config_rs_path.display()
    );
    println!("cargo:rerun-if-changed={}", target_config_path.display());
    // Declare env vars that affect the build output so parallel platform
    // builds each get their own build-script fingerprint and OUT_DIR.
    println!("cargo:rerun-if-env-changed=K_PLAT_NAME");
    println!("cargo:rerun-if-env-changed=TARGET_DIR");
}
