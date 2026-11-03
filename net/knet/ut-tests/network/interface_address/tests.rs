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
        no_prefix_route: false,
        label: None,
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
        no_prefix_route: false,
        label: None,
        ..second
    };
    assert_eq!(addresses.replace(second.addr, updated), Ok(vec![second]));
    // The replacement is reinserted at Linux's scope-derived position: its
    // host scope is narrower than `first`'s universe scope, so it goes first.
    assert_eq!(addresses.as_slice(), &[updated, first]);
    let replacement = Ipv4AddrEntry {
        addr: missing.addr,
        ..updated
    };
    assert_eq!(
        addresses.replace(second.addr, replacement),
        Ok(vec![updated])
    );
    assert_eq!(addresses.as_slice(), &[replacement, first]);
    assert!(!addresses.contains_entry(second.addr));
}

#[def_test]
fn insertion_validates_unicast_and_preserves_existing_attributes() {
    let mut addresses = Ipv4AddressSet::default();
    let primary = entry([192, 0, 2, 7], 24);
    // A zero local address is ignored before uniqueness or notification work.
    assert_eq!(addresses.insert(entry([0, 0, 0, 0], 32)), Ok(()));
    assert!(addresses.as_slice().is_empty());
    assert_eq!(addresses.insert(primary), Ok(()));
    // A duplicate identity is rejected before scope validation.
    assert_eq!(
        addresses.insert(Ipv4AddrEntry {
            scope: 254,
            ..primary
        }),
        Err(LinuxError::EEXIST)
    );
    // A different local address in the same subnet must use the primary's scope.
    let conflict = entry([192, 0, 2, 8], 24);
    assert_eq!(
        addresses.insert(Ipv4AddrEntry {
            scope: 254,
            ..conflict
        }),
        Err(LinuxError::EINVAL)
    );
    assert_eq!(addresses.as_slice(), &[primary]);
}

#[def_test]
fn source_selection_prefers_a_covering_primary_then_the_first_eligible_one() {
    let mut addresses = Ipv4AddressSet::default();
    let first = entry([192, 0, 2, 7], 24);
    let second = entry([198, 51, 100, 7], 24);
    addresses.insert(first).unwrap();
    addresses.insert(second).unwrap();
    let covering = Ipv4Address::new(198, 51, 100, 8).into();
    let unmatched = Ipv4Address::new(203, 0, 113, 8).into();
    assert_eq!(
        addresses.select_source(&covering, 0),
        Some(second.addr.address())
    );
    assert_eq!(
        addresses.select_source(&unmatched, 0),
        Some(first.addr.address())
    );
    // `inet_select_addr` caps the address scope at host scope and skips an
    // address whose effective scope exceeds the requested route scope.
    addresses
        .replace(
            first.addr,
            Ipv4AddrEntry {
                scope: 254,
                ..first
            },
        )
        .unwrap();
    assert_eq!(
        addresses.select_source(&unmatched, 0),
        Some(second.addr.address())
    );
    assert_eq!(addresses.remove(second.addr), Some(second));
    assert_eq!(
        addresses.select_source(&Ipv4Address::new(192, 0, 2, 8).into(), 254),
        Some(first.addr.address())
    );
}

#[def_test]
fn secondary_addresses_are_derived_ordered_and_removed_with_their_primary() {
    let primary = entry([192, 0, 2, 7], 24);
    let secondary = entry([192, 0, 2, 9], 24);
    let mut addresses = Ipv4AddressSet::default();
    addresses.insert(primary).unwrap();
    addresses.insert(secondary).unwrap();
    assert!(!addresses.is_secondary(primary.addr));
    assert!(addresses.is_secondary(secondary.addr));
    assert_eq!(addresses.primary_for(&secondary), Some(primary));
    assert_eq!(
        addresses.select_source(&Ipv4Address::new(192, 0, 2, 8).into(), 0),
        Some(primary.addr.address())
    );
    // Deleting a primary cascades over following addresses in the same subnet
    // with the same mask, and preserves the order of every returned entry.
    // Linux unlinks and announces secondaries before the primary.
    assert_eq!(
        addresses.remove_with_secondaries(primary.addr),
        vec![secondary, primary]
    );
    assert!(addresses.as_slice().is_empty());
}

#[def_test]
fn insertion_rejects_scope_conflicts_and_places_primaries_by_scope() {
    let mut addresses = Ipv4AddressSet::default();
    let host = Ipv4AddrEntry {
        scope: 254,
        ..entry([192, 0, 2, 7], 24)
    };
    let link = Ipv4AddrEntry {
        scope: 253,
        ..entry([198, 51, 100, 7], 24)
    };
    addresses.insert(host).unwrap();
    addresses.insert(link).unwrap();
    // Linux inserts after the last primary satisfying
    // `new_scope <= existing_scope`, so the host-scope entry stays first.
    assert_eq!(addresses.as_slice(), &[host, link]);
    assert_eq!(
        addresses.insert(Ipv4AddrEntry {
            scope: 0,
            ..entry([192, 0, 2, 8], 24)
        }),
        Err(LinuxError::EINVAL)
    );
}
