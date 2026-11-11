// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Firmware-derived RISC-V identity. S-mode must not probe M-mode CSRs.

use kcpu_id_map::LogicalCpuId;

/// Selected usable ISA extensions, with unsupported vector state omitted.
#[derive(Clone, Copy)]
#[repr(u8)]
pub enum CpuFeature {
    /// Base integer instruction set.
    I,
    /// Integer multiply/divide.
    M,
    /// Atomic instructions.
    A,
    /// Single-precision floating point with kernel context preservation.
    F,
    /// Double-precision floating point with kernel context preservation.
    D,
    /// Compressed instructions.
    C,
    /// Control/status register instructions.
    Zicsr,
    /// Instruction-fetch fence.
    Zifencei,
    /// Address-generation instructions.
    Zba,
    /// Basic bit manipulation.
    Zbb,
    /// Single-bit instructions.
    Zbs,
}

/// Immutable firmware architecture IDs and filtered ISA-extension capabilities.
#[derive(Clone, Copy)]
pub struct CpuInfo {
    architecture_ids: [Option<u64>; 3],
    capabilities: u16,
}

pub(super) fn init_cpu_info(cpu_id: LogicalCpuId, node: of::FdtNode<'_, '_>) {
    crate::cpuinfo::publish_cpu_info(cpu_id, || {
        let mut info = CpuInfo {
            architecture_ids: [None; 3],
            capabilities: 0,
        };
        for (slot, name) in info.architecture_ids.iter_mut().zip([
            "riscv,mvendorid",
            "riscv,marchid",
            "riscv,mimpid",
        ]) {
            *slot = node
                .property(name)
                .and_then(|property| match property.value {
                    bytes if bytes.len() == 4 => {
                        Some(u32::from_be_bytes(bytes.try_into().ok()?) as u64)
                    }
                    bytes if bytes.len() == 8 => Some(u64::from_be_bytes(bytes.try_into().ok()?)),
                    _ => None,
                });
        }
        if let Some(isa) = node.property_str("riscv,isa") {
            info.add_legacy_isa(isa);
        }
        if let Some(base) = node.property_str("riscv,isa-base") {
            info.add_legacy_isa(base);
        }
        if let Some(extensions) = node.property("riscv,isa-extensions") {
            info.add_extension_list(extensions.value);
        }
        info.apply_state_policy(cfg!(feature = "fp-simd"));
        info
    });
}

impl CpuInfo {
    fn add_legacy_isa(&mut self, isa: &str) {
        let prefix = if cfg!(target_pointer_width = "64") {
            "rv64"
        } else {
            "rv32"
        };
        let Some(extensions) = isa.strip_prefix(prefix) else {
            return;
        };
        if !extensions
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return;
        }
        let mut groups = extensions.split('_');
        if let Some(base) = groups.next() {
            if !base.starts_with(['i', 'g'])
                || !base
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || b"imafdcgbqvhnsup".contains(&byte))
            {
                return;
            }
            for byte in base.bytes() {
                if byte == b'g' {
                    for name in ["i", "m", "a", "f", "d", "zicsr", "zifencei"] {
                        self.add_extension(name);
                    }
                } else if byte.is_ascii_lowercase() && byte != b'p' {
                    let letter = [byte];
                    if let Ok(name) = core::str::from_utf8(&letter) {
                        self.add_extension(name);
                    }
                }
            }
        }
        for name in groups {
            // ISA versions are decimal numbers separated by 'p'; only the
            // extension name before the version participates in capability lookup.
            let end = name
                .find(|byte: char| byte.is_ascii_digit())
                .unwrap_or(name.len());
            self.add_extension(&name[..end]);
        }
    }

    fn add_extension_list(&mut self, bytes: &[u8]) {
        // Device-tree string lists must terminate every element with NUL.
        if bytes.last() != Some(&0) {
            return;
        }
        for name in bytes.split(|byte| *byte == 0) {
            if let Ok(name) = core::str::from_utf8(name) {
                self.add_extension(name);
            }
        }
    }

    fn add_extension(&mut self, name: &str) {
        let feature = match name {
            "i" => CpuFeature::I,
            "m" => CpuFeature::M,
            "a" => CpuFeature::A,
            "f" => CpuFeature::F,
            "d" => CpuFeature::D,
            "c" => CpuFeature::C,
            "zicsr" => CpuFeature::Zicsr,
            "zifencei" => CpuFeature::Zifencei,
            "zba" => CpuFeature::Zba,
            "zbb" => CpuFeature::Zbb,
            "zbs" => CpuFeature::Zbs,
            _ => return,
        };
        self.capabilities |= 1 << feature as u8;
    }

    fn apply_state_policy(&mut self, has_fp_context: bool) {
        if !self.has_feature(CpuFeature::I) {
            self.capabilities = 0;
            return;
        }
        // The current RISC-V context routines use D instructions, so an F-only
        // hart cannot use this context format. Unknown or unsupported extensions
        // (in particular V and Zve*) are never copied into the selected bitmap.
        if !has_fp_context || !self.has_feature(CpuFeature::F) || !self.has_feature(CpuFeature::D) {
            self.capabilities &= !((1 << CpuFeature::F as u8) | (1 << CpuFeature::D as u8));
        }
    }

    /// Whether firmware advertises the extension and kernel state policy allows it.
    pub fn has_feature(&self, feature: CpuFeature) -> bool {
        self.capabilities & (1 << feature as u8) != 0
    }

    /// Firmware mvendorid, marchid and mimpid; absent properties remain `None`.
    pub fn architecture_ids(&self) -> [Option<u64>; 3] {
        self.architecture_ids
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/riscv_cpuinfo.rs");
