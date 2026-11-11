// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! LoongArch CPUCFG identity and capabilities supported by the kernel.

use loongArch64::cpu::CPUCFG;

/// Selected usable capabilities; LSX/LASX are omitted without vector saving.
#[derive(Clone, Copy)]
#[repr(u8)]
pub enum CpuFeature {
    /// CPU configuration instruction.
    Cpucfg,
    /// Scalar floating point with kernel context preservation.
    Fpu,
    /// CRC32 instructions.
    Crc32,
    /// Atomic memory instructions.
    Lam,
    /// Hardware unaligned memory access.
    Ual,
}

/// Immutable CPUCFG identity and a filtered subset of instruction capabilities.
#[derive(Clone, Copy)]
pub struct CpuInfo {
    processor_id: u32,
    capabilities: u8,
}

impl CpuInfo {
    pub(super) fn detect() -> Self {
        Self::from_registers(
            CPUCFG::read(0).get_bits(0, 31) as u32,
            CPUCFG::read(1).get_bits(0, 31) as u32,
            CPUCFG::read(2).get_bits(0, 31) as u32,
            cfg!(feature = "fp-simd"),
        )
    }

    fn from_registers(processor_id: u32, cfg1: u32, cfg2: u32, has_fp_context: bool) -> Self {
        let mut info = Self {
            processor_id,
            capabilities: 0,
        };
        // Linux v6.12 arch/loongarch/include/asm/loongarch.h defines CPUCFG bits.
        for (feature, is_usable) in [
            (CpuFeature::Cpucfg, true),
            (CpuFeature::Fpu, has_fp_context && cfg2 & 7 == 7),
            (CpuFeature::Crc32, cfg1 & (1 << 25) != 0),
            (CpuFeature::Lam, cfg2 & (1 << 22) != 0),
            (CpuFeature::Ual, cfg1 & (1 << 20) != 0),
        ] {
            if is_usable {
                info.capabilities |= 1 << feature as u8;
            }
        }
        info
    }

    /// Whether this CPU offers the capability under the kernel's state policy.
    pub fn has_feature(&self, feature: CpuFeature) -> bool {
        self.capabilities & (1 << feature as u8) != 0
    }

    /// CPUCFG processor identifier, including the low-byte revision.
    pub fn processor_id(&self) -> u32 {
        self.processor_id
    }

    /// CPU family for the known Loongson implementer; unknown IDs stay unknown.
    pub fn family(&self) -> &str {
        if self.processor_id & 0x00ff_0000 == 0x0014_0000 {
            "Loongson-64bit"
        } else {
            "unknown"
        }
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/loongarch64_cpuinfo.rs");
