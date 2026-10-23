// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::{device::LoopbackDevice, network::router::ROUTE_SCOPE_UNIVERSE};

#[def_test]
fn address_capacity_is_enforced_and_released_by_interface_lifecycle() {
    let mut interfaces = DeviceRegistry::new(1);
    let dev = interfaces.register(Box::new(LoopbackDevice::new())).unwrap();
    let entry = Ipv4AddrEntry {
        addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 1), 24).unwrap(),
        scope: ROUTE_SCOPE_UNIVERSE,
        broadcast: None,
    };
    let other = Ipv4AddrEntry {
        addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 2), 24).unwrap(),
        ..entry
    };
    let _change = interfaces.add_ipv4_addr(dev, entry).unwrap();
    assert!(matches!(
        interfaces.add_ipv4_addr(dev, Ipv4AddrEntry { scope: crate::network::router::ROUTE_SCOPE_HOST, ..entry }).unwrap(),
        Ipv4AddrChange::Unchanged
    ));
    assert_eq!(interfaces.ipv4(dev).unwrap().as_slice(), &[entry]);
    assert!(matches!(interfaces.add_ipv4_addr(dev, other), Err(LinuxError::ENOBUFS)));
    assert!(matches!(
        interfaces.replace_ipv4_addr(dev, entry.addr, other).unwrap(),
        Ipv4AddrChange::Updated { old_source: Some(source) } if source == entry.addr.address()
    ));
    assert_eq!(interfaces.ipv4(dev).unwrap().as_slice(), &[other]);
    let slot = interfaces.slot(dev).unwrap();
    assert_eq!(interfaces.remove_slot(slot), alloc::vec![other.addr.address()]);
    assert!(interfaces.ipv4(dev).is_none());
    assert!(matches!(interfaces.add_ipv4_addr(dev, entry), Err(LinuxError::ENODEV)));
    let dev = interfaces.register(Box::new(LoopbackDevice::new())).unwrap();
    let _change = interfaces.add_ipv4_addr(dev, entry).unwrap();
    assert_eq!(interfaces.ipv4(dev).unwrap().as_slice(), &[entry]);
}

#[def_test]
fn exact_replacement_preserves_primary_and_rejects_collisions() {
    let mut devices = DeviceRegistry::new(2);
    let dev = devices.register(Box::new(LoopbackDevice::new())).unwrap();
    let primary = Ipv4AddrEntry {
        addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 1), 24).unwrap(),
        scope: ROUTE_SCOPE_UNIVERSE,
        broadcast: None,
    };
    let secondary = Ipv4AddrEntry {
        addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 2), 24).unwrap(),
        ..primary
    };
    let _change = devices.add_ipv4_addr(dev, primary).unwrap();
    let _change = devices.add_ipv4_addr(dev, secondary).unwrap();
    assert!(matches!(
        devices.replace_ipv4_addr(dev, secondary.addr, primary),
        Err(LinuxError::EEXIST)
    ));
    assert_eq!(devices.ipv4(dev).unwrap().as_slice(), &[primary, secondary]);
    let updated = Ipv4AddrEntry { broadcast: Some(Ipv4Address::new(192, 0, 2, 123)), ..secondary };
    let _change = devices.replace_ipv4_addr(dev, secondary.addr, updated).unwrap();
    assert_eq!(devices.ipv4(dev).unwrap().as_slice(), &[primary, updated]);
}
