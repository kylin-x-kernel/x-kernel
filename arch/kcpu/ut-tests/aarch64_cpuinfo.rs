// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::{CpuFeature, CpuInfo};

#[def_test]
fn midr_fields_and_simd_dependent_crypto_are_filtered() {
    let isar0 = (2 << 4) | (1 << 8) | (2 << 12) | (1 << 16) | (2 << 20);
    let info = CpuInfo::from_registers(0x413f_d034, 0, isar0, true);
    assert_eq!(info.identity(), (0x41, 3, 0xd03, 4));
    for feature in [
        CpuFeature::Fp,
        CpuFeature::Asimd,
        CpuFeature::Aes,
        CpuFeature::Pmull,
        CpuFeature::Sha1,
        CpuFeature::Sha2,
        CpuFeature::Crc32,
        CpuFeature::Atomics,
    ] {
        assert!(info.has_feature(feature));
    }
    for (pfr0, has_fp_context) in [(0, false), (0xff << 16, true)] {
        let info = CpuInfo::from_registers(0, pfr0, isar0, has_fp_context);
        for feature in [
            CpuFeature::Fp,
            CpuFeature::Asimd,
            CpuFeature::Aes,
            CpuFeature::Pmull,
            CpuFeature::Sha1,
            CpuFeature::Sha2,
        ] {
            assert!(!info.has_feature(feature));
        }
        assert!(info.has_feature(CpuFeature::Crc32));
        assert!(info.has_feature(CpuFeature::Atomics));
    }
    let reserved = CpuInfo::from_registers(0, u64::MAX, u64::MAX, true);
    assert!(!reserved.has_feature(CpuFeature::Fp));
    assert!(!reserved.has_feature(CpuFeature::Aes));
    assert!(!reserved.has_feature(CpuFeature::Atomics));
}

/// The `AT_HWCAP` word user runtimes read must use Linux's bit layout, not the
/// local `CpuFeature` discriminant order. AES is bit 3 because bit 2 is
/// ASIMDHP; a word shifted by that difference makes glibc's IFUNC resolvers and
/// LLVM's `getHostCPUFeatures` (behind llvmpipe's JIT) pick instruction paths
/// the running CPU may not implement.
#[def_test]
fn hwcap_uses_linux_bit_positions() {
    // Every feature usable: FP|ASIMD|AES|PMULL|SHA1|SHA2|CRC32|ATOMICS.
    let isar0 = (2 << 4) | (1 << 8) | (2 << 12) | (1 << 16) | (2 << 20);
    let all = CpuInfo::from_registers(0x413f_d034, 0, isar0, true);
    assert_eq!(all.hwcap(), 0x1fb);

    // No FP/SIMD context: FP, ASIMD, AES, PMULL, SHA1 and SHA2 drop out while
    // CRC32 and atomics stay, exactly as the capability set does.
    let no_simd = CpuInfo::from_registers(0, 0, isar0, false);
    assert_eq!(no_simd.hwcap(), (1 << 7) | (1 << 8));

    // An all-zero capability set publishes zero, never a guess.
    let none = CpuInfo::from_registers(0, 0xff << 16, 0, true);
    assert_eq!(none.hwcap(), 0);
}

/// The published word must be safe on every core a task may migrate to.
///
/// Calls the same fold `system_hwcap` uses: a test that re-implemented the
/// intersection would pass for any implementation of the real one.
#[def_test]
fn system_hwcap_intersects_the_online_cpus() {
    let isar0 = (2 << 4) | (1 << 8) | (2 << 12) | (1 << 16) | (2 << 20);
    let strong = CpuInfo::from_registers(0x413f_d034, 0, isar0, true);
    // Same crypto/atomics, but no preserved FP/SIMD context.
    let weak = CpuInfo::from_registers(0x413f_d034, 0, isar0, false);
    let shared = (1 << 7) | (1 << 8);
    let fold = |words: &[usize]| crate::intersect_hwcap(words.iter().copied());

    // Both cores online: only what both advertise survives.
    assert_eq!(fold(&[strong.hwcap(), weak.hwcap()]), shared);
    assert_eq!(fold(&[weak.hwcap(), strong.hwcap()]), shared);
    // A single online core publishes its own word unchanged.
    assert_eq!(fold(&[strong.hwcap()]), strong.hwcap());
    // No identified CPU yet: claim nothing.
    assert_eq!(fold(&[]), 0);
    // The asymmetric bit must actually be dropped, not merely differ.
    assert_eq!(
        strong.hwcap() & !shared,
        (1 << 0) | (1 << 1) | (1 << 3) | (1 << 4) | (1 << 5) | (1 << 6)
    );
}
