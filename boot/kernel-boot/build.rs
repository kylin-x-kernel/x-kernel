// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{io::Result, path::Path};

fn main() {
    println!("cargo:rerun-if-changed=linker.lds.S");
    println!("cargo:rerun-if-changed=build.rs");
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let workspace_root = Path::new(&manifest_dir).parent().unwrap().parent().unwrap();
    let target_config_path = workspace_root.join("target/kbuild/config.rs");
    println!("cargo:rerun-if-changed={}", target_config_path.display());
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let platform = kbuild_config::PLATFORM;
    if platform != "unknown" {
        gen_linker_script(&arch, platform).unwrap();
    }
}

/// Generates a linker script for the given target arch and platform.
fn gen_linker_script(arch: &str, platform: &str) -> Result<()> {
    let fname = format!("linker_{platform}.lds");
    let output_arch = if arch == "x86_64" {
        "i386:x86-64"
    } else if arch.contains("riscv") {
        "riscv" // OUTPUT_ARCH of both riscv32/riscv64 is "riscv"
    } else {
        arch
    };
    let ld_content = std::fs::read_to_string("linker.lds.S")?;
    let ld_content = ld_content.replace("%ARCH%", output_arch);
    let layout = kaddr_layout::for_arch(arch);
    let ld_content = ld_content.replace("%KIMAGE_VADDR%", &format!("{:#x}", layout.kimage_vaddr));

    let ld_content = ld_content.replace("%CPU_NUM%", &format!("{}", kbuild_config::CPU_NUM));
    let ld_content = ld_content.replace(
        "%DWARF%",
        if kbuild_config::KFEAT_DWARF {
            r#"debug_abbrev : { . += SIZEOF(.debug_abbrev); }
    debug_addr : { . += SIZEOF(.debug_addr); }
    debug_aranges : { . += SIZEOF(.debug_aranges); }
    debug_info : { . += SIZEOF(.debug_info); }
    debug_line : { . += SIZEOF(.debug_line); }
    debug_line_str : { . += SIZEOF(.debug_line_str); }
    debug_ranges : { . += SIZEOF(.debug_ranges); }
    debug_rnglists : { . += SIZEOF(.debug_rnglists); }
    debug_str : { . += SIZEOF(.debug_str); }
    debug_str_offsets : { . += SIZEOF(.debug_str_offsets); }"#
        } else {
            ""
        },
    );

    // target/<target_triple>/<mode>/build/khal-xxxx/out
    let out_dir = std::env::var("OUT_DIR").unwrap();
    // target/<target_triple>/<mode>/linker_xxxx.lds
    let out_path = Path::new(&out_dir).join("../../..").join(fname);
    // Tell cargo to re-run this script if the output linker script is missing.
    println!("cargo:rerun-if-changed={}", out_path.display());
    std::fs::write(out_path, ld_content)?;
    Ok(())
}
