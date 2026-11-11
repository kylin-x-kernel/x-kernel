// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::{CpuFeature, CpuInfo};

#[def_test]
fn cpucfg_identity_and_scalar_state_policy_are_independent() {
    let info = CpuInfo::from_registers(0x0014_c012, (1 << 25) | (1 << 20), u32::MAX, true);
    assert_eq!(info.processor_id(), 0x0014_c012);
    assert_eq!(info.family(), "Loongson-64bit");
    for feature in [
        CpuFeature::Cpucfg,
        CpuFeature::Fpu,
        CpuFeature::Crc32,
        CpuFeature::Lam,
        CpuFeature::Ual,
    ] {
        assert!(info.has_feature(feature));
    }
    let info = CpuInfo::from_registers(0, 1 << 25, u32::MAX, false);
    assert_eq!(info.family(), "unknown");
    assert!(!info.has_feature(CpuFeature::Fpu));
    assert!(info.has_feature(CpuFeature::Crc32));
    assert!(!CpuInfo::from_registers(0, 0, 1, true).has_feature(CpuFeature::Fpu));
}
