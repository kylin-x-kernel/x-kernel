// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn inet_abc_len_uses_classful_defaults() {
    assert_eq!(inet_abc_len(Ipv4Addr::UNSPECIFIED), Some(0));
    assert_eq!(inet_abc_len(Ipv4Addr::BROADCAST), Some(0));
    assert_eq!(inet_abc_len(Ipv4Addr::new(0, 1, 2, 3)), Some(8));
    assert_eq!(inet_abc_len(Ipv4Addr::new(10, 0, 2, 15)), Some(8));
    assert_eq!(inet_abc_len(Ipv4Addr::new(172, 16, 0, 1)), Some(16));
    assert_eq!(inet_abc_len(Ipv4Addr::new(192, 168, 1, 1)), Some(24));
    assert_eq!(inet_abc_len(Ipv4Addr::new(224, 0, 0, 1)), None);
    assert_eq!(inet_abc_len(Ipv4Addr::new(240, 0, 0, 1)), Some(32));
}

#[def_test]
fn prefix_from_netmask_rejects_non_contiguous_masks() {
    assert_eq!(prefix_from_netmask(Ipv4Addr::UNSPECIFIED), Some(0));
    assert_eq!(
        prefix_from_netmask(Ipv4Addr::new(255, 255, 255, 0)),
        Some(24)
    );
    assert_eq!(prefix_from_netmask(Ipv4Addr::new(255, 255, 0, 255)), None);
    assert_eq!(core::mem::size_of::<Rtentry>(), 120);
}

#[def_test]
fn extract_inet_ipv4_requires_af_inet() {
    let mut data = [0u8; 24];
    data[0..2].copy_from_slice(&(AF_INET as u16).to_ne_bytes());
    data[4..8].copy_from_slice(&[10, 0, 2, 15]);
    assert_eq!(
        extract_inet_ipv4(&data).unwrap(),
        Ipv4Addr::new(10, 0, 2, 15)
    );

    data[0..2].copy_from_slice(&0u16.to_ne_bytes());
    assert_eq!(
        extract_inet_ipv4(&data).unwrap_err(),
        KError::from(LinuxError::EINVAL)
    );
}

#[def_test]
fn route_delete_treats_zero_gateway_as_wildcard() {
    let gateway = Sockaddr {
        family: AF_INET as u16,
        data: [0; 14],
    };
    assert_eq!(
        parse_route_gateway(RTF_GATEWAY, gateway, RouteOperation::Delete).unwrap(),
        None
    );
    assert_eq!(
        parse_route_gateway(RTF_GATEWAY, gateway, RouteOperation::Add).unwrap_err(),
        KError::from(LinuxError::EINVAL)
    );
}

#[def_test]
fn udhcpc_ioctls_include_broadcast_and_route_delete() {
    assert!(is_net_ioctl_cmd(SIOCSIFBRDADDR));
    assert!(is_net_ioctl_cmd(SIOCDELRT));
    assert!(is_net_ioctl_write(SIOCSIFBRDADDR));
    assert!(is_net_ioctl_write(SIOCDELRT));
}
