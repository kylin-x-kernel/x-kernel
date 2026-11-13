// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AArch64 identification and the supported subset of usable EL0 features.

use aarch64_cpu::registers::{ID_AA64ISAR0_EL1, ID_AA64PFR0_EL1, MIDR_EL1, Readable};

/// Selected usable capabilities; procfs owns their Linux spellings.
#[derive(Clone, Copy)]
#[repr(u8)]
pub enum CpuFeature {
    /// Scalar floating point with kernel context preservation.
    Fp,
    /// Advanced SIMD with kernel context preservation.
    Asimd,
    /// AES instructions.
    Aes,
    /// Polynomial multiplication instructions.
    Pmull,
    /// SHA-1 instructions.
    Sha1,
    /// SHA-256 instructions.
    Sha2,
    /// CRC32 instructions, independent of FP/SIMD state.
    Crc32,
    /// Large System Extensions atomic instructions.
    Atomics,
}

/// Immutable MIDR identity and filtered instruction capabilities of one CPU.
#[derive(Clone, Copy)]
pub struct CpuInfo {
    midr: u64,
    capabilities: u16,
}

impl CpuInfo {
    pub(super) fn detect() -> Self {
        Self::from_registers(
            MIDR_EL1.get(),
            ID_AA64PFR0_EL1.get(),
            ID_AA64ISAR0_EL1.get(),
            cfg!(feature = "fp-simd"),
        )
    }

    fn from_registers(midr: u64, pfr0: u64, isar0: u64, has_fp_context: bool) -> Self {
        let field = |register: u64, shift: u32| (register >> shift) & 0xf;
        let mut info = Self {
            midr,
            capabilities: 0,
        };
        let has_fp = has_fp_context && field(pfr0, 16) <= 1;
        let has_simd = has_fp && field(pfr0, 20) <= 1;
        for (feature, is_usable) in [
            (CpuFeature::Fp, has_fp),
            (CpuFeature::Asimd, has_simd),
            (
                CpuFeature::Aes,
                has_simd && matches!(field(isar0, 4), 1 | 2),
            ),
            (CpuFeature::Pmull, has_simd && field(isar0, 4) == 2),
            (CpuFeature::Sha1, has_simd && field(isar0, 8) == 1),
            (
                CpuFeature::Sha2,
                has_simd && matches!(field(isar0, 12), 1 | 2),
            ),
            (CpuFeature::Crc32, field(isar0, 16) == 1),
            (CpuFeature::Atomics, matches!(field(isar0, 20), 2 | 3)),
        ] {
            if is_usable {
                info.capabilities |= 1 << feature as u8;
            }
        }
        info
    }

    /// Whether hardware and kernel context policy permit this capability.
    pub fn has_feature(&self, feature: CpuFeature) -> bool {
        self.capabilities & (1 << feature as u8) != 0
    }

    /// Userspace-visible capability word in the Linux `AT_HWCAP` encoding.
    ///
    /// User runtimes read this instead of probing control registers: glibc's
    /// IFUNC resolvers and LLVM's `getHostCPUFeatures` (behind llvmpipe's JIT)
    /// select instruction paths from it. The bit positions are Linux's
    /// (`arch/arm64/include/uapi/asm/hwcap.h`), not this crate's
    /// `CpuFeature` discriminant order: AES is bit 3 because bit 2 is ASIMDHP,
    /// which this kernel does not publish. Only features the kernel deems
    /// usable are set, so a CPU whose FP/SIMD state is not preserved never
    /// advertises it.
    pub fn hwcap(&self) -> usize {
        const HWCAP_FP: usize = 1 << 0;
        const HWCAP_ASIMD: usize = 1 << 1;
        const HWCAP_AES: usize = 1 << 3;
        const HWCAP_PMULL: usize = 1 << 4;
        const HWCAP_SHA1: usize = 1 << 5;
        const HWCAP_SHA2: usize = 1 << 6;
        const HWCAP_CRC32: usize = 1 << 7;
        const HWCAP_ATOMICS: usize = 1 << 8;

        [
            (CpuFeature::Fp, HWCAP_FP),
            (CpuFeature::Asimd, HWCAP_ASIMD),
            (CpuFeature::Aes, HWCAP_AES),
            (CpuFeature::Pmull, HWCAP_PMULL),
            (CpuFeature::Sha1, HWCAP_SHA1),
            (CpuFeature::Sha2, HWCAP_SHA2),
            (CpuFeature::Crc32, HWCAP_CRC32),
            (CpuFeature::Atomics, HWCAP_ATOMICS),
        ]
        .into_iter()
        .filter(|(feature, _)| self.has_feature(*feature))
        .fold(0, |hwcap, (_, bit)| hwcap | bit)
    }

    /// MIDR implementer, variant, part number and revision, respectively.
    pub fn identity(&self) -> (u8, u8, u16, u8) {
        (
            (self.midr >> 24) as u8,
            ((self.midr >> 20) & 0xf) as u8,
            ((self.midr >> 4) & 0xfff) as u16,
            (self.midr & 0xf) as u8,
        )
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/aarch64_cpuinfo.rs");
