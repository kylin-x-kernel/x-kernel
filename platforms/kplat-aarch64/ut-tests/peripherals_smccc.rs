// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn test_smccc_conduit_raw_values() {
    assert_eq!(
        SmcccConduit::from_raw(SmcccConduit::None as u8),
        SmcccConduit::None
    );
    assert_eq!(
        SmcccConduit::from_raw(SmcccConduit::Hvc as u8),
        SmcccConduit::Hvc
    );
    assert_eq!(
        SmcccConduit::from_raw(SmcccConduit::Smc as u8),
        SmcccConduit::Smc
    );
    assert_eq!(SmcccConduit::from_raw(0xff), SmcccConduit::None);
}

#[def_test]
fn test_smccc_conduit_names() {
    assert_eq!(SmcccConduit::None.as_str(), "none");
    assert_eq!(SmcccConduit::Hvc.as_str(), "hvc");
    assert_eq!(SmcccConduit::Smc.as_str(), "smc");
}

#[def_test]
fn test_standard_conduit_preserves_none_and_smc() {
    assert_eq!(standard_conduit_for(SmcccConduit::None), SmcccConduit::None);
    assert_eq!(standard_conduit_for(SmcccConduit::Smc), SmcccConduit::Smc);
}

#[def_test]
fn test_standard_conduit_overrides_hvc_only_for_vmm() {
    let expected = if cfg!(feature = "vmm") {
        SmcccConduit::Smc
    } else {
        SmcccConduit::Hvc
    };
    assert_eq!(standard_conduit_for(SmcccConduit::Hvc), expected);
}
