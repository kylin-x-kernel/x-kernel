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
        "x86_64" => "x86_64-qemu-virt",
        "aarch64" => "aarch64-qemu-virt",
        "riscv64" => "riscv64-qemu-virt",
        "loongarch64" => "loongarch64-qemu-virt",
        _ => "aarch64-qemu-virt",
    };
    let plat_name = env::var("K_PLAT_NAME").unwrap_or_else(|_| default_plat_name.to_string());
    let target_config_path = Path::new(&target_dir)
        .join("kbuild")
        .join(plat_name)
        .join("config.rs");

    if target_config_path.exists() {
        // Copy the generated config
        let config_content =
            fs::read_to_string(&target_config_path).expect("Failed to read generated config.rs");
        fs::write(&config_rs_path, config_content).expect("Failed to write config.rs to OUT_DIR");
    } else {
        // Generate empty config if not available
        fs::write(&config_rs_path, "// No config.rs generated yet\n")
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
