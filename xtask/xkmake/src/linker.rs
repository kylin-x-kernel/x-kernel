// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel linker-script generation.

use std::{fs, path::Path};

use xconfig::build_config::{KernelArch, ResolvedKernelConfig};

use crate::error::{Error, IoResultExt, Result};

const LINKER_TEMPLATE: &str = include_str!("../../../linker.lds.S");
const DWARF_SECTIONS: &str = r#"debug_abbrev : { . += SIZEOF(.debug_abbrev); }
    debug_addr : { . += SIZEOF(.debug_addr); }
    debug_aranges : { . += SIZEOF(.debug_aranges); }
    debug_info : { . += SIZEOF(.debug_info); }
    debug_line : { . += SIZEOF(.debug_line); }
    debug_line_str : { . += SIZEOF(.debug_line_str); }
    debug_ranges : { . += SIZEOF(.debug_ranges); }
    debug_rnglists : { . += SIZEOF(.debug_rnglists); }
    debug_str : { . += SIZEOF(.debug_str); }
    debug_str_offsets : { . += SIZEOF(.debug_str_offsets); }"#;

pub(crate) fn generate(
    config: &ResolvedKernelConfig,
    output_path: &Path,
    verbosity: u8,
) -> Result<()> {
    let content = render(
        config.arch(),
        config.nr_cpus(),
        config.is_enabled("KFEAT_DWARF"),
    );
    let parent = output_path.parent().ok_or_else(|| {
        Error::Message(format!(
            "linker script has no parent directory: {}",
            output_path.display()
        ))
    })?;
    fs::create_dir_all(parent).with_path(parent)?;

    if write_if_changed(output_path, content.as_bytes())? && verbosity > 0 {
        println!("Generated linker script {}", output_path.display());
    }
    Ok(())
}

fn render(arch: KernelArch, nr_cpus: usize, dwarf: bool) -> String {
    LINKER_TEMPLATE
        .replace("%ARCH%", output_arch(arch))
        .replace(
            "%KIMAGE_VADDR%",
            &format!("{:#x}", kaddr_layout::for_arch(arch.as_str()).kimage_vaddr),
        )
        .replace("%NR_CPUS%", &nr_cpus.to_string())
        .replace(
            "%BUILD_INFO_SIZE%",
            &kernel_image_metadata::BUILD_INFO_DESCRIPTOR_SIZE.to_string(),
        )
        .replace("%DWARF%", if dwarf { DWARF_SECTIONS } else { "" })
}

const fn output_arch(arch: KernelArch) -> &'static str {
    match arch {
        KernelArch::X86_64 => "i386:x86-64",
        KernelArch::Riscv64 => "riscv",
        _ => arch.as_str(),
    }
}

fn write_if_changed(path: &Path, content: &[u8]) -> Result<bool> {
    match fs::read(path) {
        Ok(existing) if existing == content => return Ok(false),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(Error::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    }

    fs::write(path, content).with_path(path)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_architecture_and_build_inputs() {
        let content = render(KernelArch::Riscv64, 8, true);

        assert!(content.starts_with("OUTPUT_ARCH(riscv)"));
        assert!(content.contains("BASE_ADDRESS = 0xffffffe000000000;"));
        assert!(content.contains("ALIGN(64) * 8;"));
        assert!(content.contains("debug_info : { . += SIZEOF(.debug_info); }"));
        assert!(!content.contains("%ARCH%"));
        assert!(!content.contains("%KIMAGE_VADDR%"));
        assert!(!content.contains("%NR_CPUS%"));
        assert!(!content.contains("%BUILD_INFO_SIZE%"));
        assert!(!content.contains("%DWARF%"));
    }

    #[test]
    fn unchanged_content_is_not_rewritten() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("linker.lds");

        assert!(write_if_changed(&path, b"content\n").unwrap());
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        assert!(!write_if_changed(&path, b"content\n").unwrap());
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
    }
}
