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
