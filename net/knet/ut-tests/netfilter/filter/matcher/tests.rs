// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn match_value_constructors_validate_ranges_and_masks() {
    assert!(PortRange::new(80, 443).is_some());
    assert_eq!(PortRange::new(443, 80), None);
    assert!(TcpFlagsCondition::new(0x12, 0x02).is_some());
    assert_eq!(TcpFlagsCondition::new(0x02, 0x12), None);
    assert!(IcmpMatch::new(Some(3), 0, 15).is_some());
    assert_eq!(IcmpMatch::new(Some(3), 15, 0), None);
    assert_eq!(
        InterfacePattern::exact("X".repeat(MAX_INTERFACE_NAME_LEN + 1)),
        None
    );
    assert!(InterfacePattern::exact("X".repeat(MAX_INTERFACE_NAME_LEN)).is_some());
    assert!(InterfacePattern::prefix("X".repeat(MAX_INTERFACE_NAME_LEN - 1)).is_some());
    assert!(InterfacePattern::prefix("X".repeat(MAX_INTERFACE_NAME_LEN)).is_none());
}

#[def_test]
fn port_ranges_include_both_endpoints() {
    let range = PortRange::new(80, 443).unwrap();
    assert!(!range.contains(79));
    assert!(range.contains(80));
    assert!(range.contains(443));
    assert!(!range.contains(444));

    let single_port = PortRange::new(80, 80).unwrap();
    assert!(!single_port.contains(79));
    assert!(single_port.contains(80));
    assert!(!single_port.contains(81));

    let all_ports = PortRange::new(0, u16::MAX).unwrap();
    assert!(all_ports.contains(0));
    assert!(all_ports.contains(u16::MAX));
}
