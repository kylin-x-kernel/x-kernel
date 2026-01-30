// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

fn main() {
    println!("cargo:rerun-if-env-changed=PLAT_CONFIG_PATH");
    if let Ok(config_path) = std::env::var("PLAT_CONFIG_PATH") {
        println!("cargo:rerun-if-changed={config_path}");
    }
}
