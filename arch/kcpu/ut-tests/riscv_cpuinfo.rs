// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, def_test};

use super::{CpuFeature, CpuInfo};

#[def_test]
fn firmware_isa_versions_and_g_shorthand_preserve_usable_extensions() {
    let mut info = CpuInfo {
        architecture_ids: [None; 3],
        capabilities: 0,
    };
    info.add_legacy_isa("rv64i2p1m2p0a2p1f2p2d2p2c2p0_zba1p0_zbb1p0_zbs1p0");
    info.apply_state_policy(true);
    for feature in [
        CpuFeature::I,
        CpuFeature::M,
        CpuFeature::A,
        CpuFeature::F,
        CpuFeature::D,
        CpuFeature::C,
        CpuFeature::Zba,
        CpuFeature::Zbb,
        CpuFeature::Zbs,
    ] {
        assert!(info.has_feature(feature));
    }
    let mut info = CpuInfo {
        architecture_ids: [None; 3],
        capabilities: 0,
    };
    info.add_legacy_isa("rv64gc");
    info.apply_state_policy(false);
    assert!(info.has_feature(CpuFeature::I));
    assert!(info.has_feature(CpuFeature::Zicsr));
    assert!(info.has_feature(CpuFeature::Zifencei));
    assert!(!info.has_feature(CpuFeature::F));
    assert!(!info.has_feature(CpuFeature::D));
}

#[def_test]
fn malformed_firmware_cannot_inject_or_enable_unsupported_state() {
    for isa in [
        "rv32imafdc",
        "rv64imafdc\nflags: avx",
        "rv64imafdc\t",
        "garbage",
        "rv64fakeimafdc",
        "rv64izicsr",
    ] {
        let mut info = CpuInfo {
            architecture_ids: [None; 3],
            capabilities: 0,
        };
        info.add_legacy_isa(isa);
        assert!(!info.has_feature(CpuFeature::I));
    }
    let mut info = CpuInfo {
        architecture_ids: [None; 3],
        capabilities: 0,
    };
    for name in ["i", "f", "v", "zve64d", "zbb\n"] {
        info.add_extension(name);
    }
    info.apply_state_policy(true);
    assert!(info.has_feature(CpuFeature::I));
    assert!(!info.has_feature(CpuFeature::F));
    assert!(!info.has_feature(CpuFeature::Zbb));
    let mut info = CpuInfo {
        architecture_ids: [None; 3],
        capabilities: 0,
    };
    info.add_extension_list(b"i\0m\0a\0f\0d\0c\0zbb\0v\0");
    info.apply_state_policy(true);
    assert!(info.has_feature(CpuFeature::I));
    assert!(info.has_feature(CpuFeature::D));
    assert!(info.has_feature(CpuFeature::Zbb));
    let mut info = CpuInfo {
        architecture_ids: [None; 3],
        capabilities: 0,
    };
    info.add_extension_list(b"i\0m");
    assert!(!info.has_feature(CpuFeature::I));
}
