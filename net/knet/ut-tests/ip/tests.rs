// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::string::ToString;

use unittest::def_test;

use super::*;

#[def_test]
fn test_ip_address_and_cidr_helpers() {
    let addr = Ipv4Address::new(192, 0, 2, 1);
    let cidr = Ipv4Cidr::new(addr, 24).unwrap();
    let ip_cidr = IpCidr::from(cidr);

    assert_eq!(cidr.address(), addr);
    assert_eq!(cidr.prefix_len(), 24);
    assert_eq!(cidr.broadcast(), Some(Ipv4Address::new(192, 0, 2, 255)));
    assert!(cidr.contains_addr(&IpAddress::from(addr)));
    assert_eq!(ip_cidr.address(), IpAddress::from(addr));
    assert_eq!(ip_cidr.prefix_len(), 24);
    assert!(ip_cidr.contains_addr(&IpAddress::from(addr)));
    assert!(IpAddress::from(Ipv4Address::UNSPECIFIED).is_unspecified());
    assert!(IpAddress::from(Ipv4Address::BROADCAST).is_broadcast());
    assert_eq!(Ipv4Cidr::new(addr, 33), None);
}

#[def_test]
fn test_ipv6_and_listen_endpoint_helpers() {
    let octets = [1; 16];
    let addr = Ipv6Address::from_octets(octets);
    let endpoint = IpListenEndpoint {
        addr: Some(IpAddress::from(addr)),
        port: 8080,
    };

    assert_eq!(addr.octets(), octets);
    assert!(!addr.is_unspecified());
    assert!(Ipv6Address::UNSPECIFIED.is_unspecified());
    assert_eq!(endpoint.to_string(), "101:101:101:101:101:101:101:101:8080");
}
