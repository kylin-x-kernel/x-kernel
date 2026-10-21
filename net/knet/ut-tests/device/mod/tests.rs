// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn ethernet_mtu_validation_accepts_only_supported_boundaries() {
    assert_eq!(LinkKind::Ethernet.validate_mtu(IPV4_MIN_MTU), Ok(()));
    assert_eq!(LinkKind::Ethernet.validate_mtu(ETHERNET_MAX_MTU), Ok(()));
    assert_eq!(
        LinkKind::Ethernet.validate_mtu(IPV4_MIN_MTU - 1),
        Err(LinuxError::EINVAL)
    );
    assert_eq!(
        LinkKind::Ethernet.validate_mtu(ETHERNET_MAX_MTU + 1),
        Err(LinuxError::EINVAL)
    );
}

#[def_test]
fn interface_name_rejects_linux_whitespace_bytes() {
    assert_eq!(validate_interface_name("eth\x0b1"), Err(LinuxError::EINVAL));
}
