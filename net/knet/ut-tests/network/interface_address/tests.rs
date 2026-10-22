// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::*;

fn entry(address: [u8; 4], prefix: u8) -> Ipv4AddrEntry {
    Ipv4AddrEntry {
        addr: Ipv4Cidr::new(
            Ipv4Address::new(address[0], address[1], address[2], address[3]),
            prefix,
        )
        .unwrap(),
        scope: 0,
        broadcast: None,
    }
}

#[def_test]
fn replacing_by_cidr_preserves_other_entries_and_rejects_invalid_updates() {
    let first = entry([192, 0, 2, 7], 24);
    let second = entry([192, 0, 2, 7], 16);
    let missing = entry([203, 0, 113, 7], 24);
    let mut addresses = Ipv4AddressSet::default();
    addresses.insert(first).unwrap();
    addresses.insert(second).unwrap();
    for (addr, replacement, error) in [
        (second.addr, entry([0, 0, 0, 0], 32), LinuxError::EINVAL),
        (missing.addr, missing, LinuxError::EADDRNOTAVAIL),
        (first.addr, second, LinuxError::EEXIST),
        (second.addr, first, LinuxError::EEXIST),
    ] {
        assert_eq!(addresses.replace(addr, replacement), Err(error));
        assert_eq!(addresses.as_slice(), &[first, second]);
    }
    // Legacy ioctl tests cover the first entry; exercise a later CIDR here.
    let updated = Ipv4AddrEntry {
        scope: 254,
        broadcast: Some(Ipv4Address::new(192, 0, 2, 111)),
        ..second
    };
    assert_eq!(addresses.replace(second.addr, updated), Ok(second));
    assert_eq!(addresses.as_slice(), &[first, updated]);
    let replacement = Ipv4AddrEntry { addr: missing.addr, ..updated };
    assert_eq!(addresses.replace(second.addr, replacement), Ok(updated));
    assert_eq!(addresses.as_slice(), &[first, replacement]);
    assert!(!addresses.contains_entry(second.addr));
}

#[def_test]
fn insertion_validates_unicast_and_preserves_existing_attributes() {
    let mut addresses = Ipv4AddressSet::default();
    let primary = entry([192, 0, 2, 7], 24);
    assert_eq!(
        addresses.insert(entry([0, 0, 0, 0], 32)),
        Err(LinuxError::EINVAL)
    );
    assert!(addresses.as_slice().is_empty());
    assert_eq!(addresses.insert(primary), Ok(()));
    assert_eq!(
        addresses.insert(Ipv4AddrEntry {
            scope: 254,
            ..primary
        }),
        Ok(())
    );
    assert_eq!(addresses.as_slice(), &[primary]);
}

#[def_test]
fn source_selection_prefers_covering_then_first_scope_eligible_address() {
    let mut addresses = Ipv4AddressSet::default();
    let primary = entry([192, 0, 2, 7], 24);
    let secondary = entry([198, 51, 100, 7], 24);
    addresses.insert(primary).unwrap();
    addresses.insert(secondary).unwrap();
    let next_hop = Ipv4Address::new(198, 51, 100, 8).into();
    let unmatched = Ipv4Address::new(203, 0, 113, 8).into();
    assert_eq!(
        addresses.select_source(&next_hop, 0),
        Some(secondary.addr.address())
    );
    assert_eq!(
        addresses.select_source(&unmatched, 0),
        Some(primary.addr.address())
    );
    addresses
        .replace(
            primary.addr,
            Ipv4AddrEntry {
                scope: 254,
                ..primary
            },
        )
        .unwrap();
    assert_eq!(
        addresses.select_source(&unmatched, 0),
        Some(secondary.addr.address())
    );
    assert_eq!(addresses.remove(secondary.addr), Some(secondary));
    assert_eq!(addresses.select_source(&next_hop, 0), None);
    assert_eq!(
        addresses.select_source(&next_hop, 254),
        Some(primary.addr.address())
    );
}
