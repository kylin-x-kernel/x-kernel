// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let linker_script = manifest_dir.join("linker.lds");
    println!("cargo:rustc-link-arg=-T{}", linker_script.display());
    println!("cargo:rustc-link-arg=-no-pie");
    println!("cargo:rerun-if-changed={}", linker_script.display());
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("src/boot.S").display()
    );
}
