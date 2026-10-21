// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;
use crate::ip::Ipv4Address;

fn configured_rule(dev: i32, prefix_len: u8) -> Rule {
    Rule::new(
        Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 0), prefix_len)
            .unwrap()
            .into(),
        None,
        NetDeviceId::new(dev).unwrap(),
        Ipv4Address::new(192, 0, 2, dev as u8).into(),
    )
}

#[def_test]
fn route_priority_survives_storage_changes() {
    for initial_order in [0, u64::MAX - 4] {
        let mut table = RouteTable::new();
        table.next_insertion_order = initial_order;
        let broad = configured_rule(2, 16);
        let oldest = configured_rule(3, 24);
        let newest = configured_rule(4, 24);
        for rule in [broad, oldest, newest, oldest] {
            table.add_rule(rule);
        }
        let destination = Ipv4Address::new(192, 0, 2, 0).into();
        assert_eq!(table.lookup(&destination).unwrap().dev, oldest.dev);

        // Removal and reinsertion preserve precedence, including sequence compaction.
        table.remove_exact_rule(broad);
        table.add_rule(broad);
        assert_eq!(table.lookup(&destination).unwrap().dev, oldest.dev);
        assert_eq!(table.snapshot()[0].dev, oldest.dev);
        table.remove_exact_rule(oldest);
        assert_eq!(table.lookup(&destination).unwrap().dev, newest.dev);
        table.remove_exact_rule(newest);
        table.add_rule(newest);
        assert_eq!(table.lookup(&destination).unwrap().dev, oldest.dev);

        let host = configured_rule(2, 32);
        table.add_rule(host);
        assert_eq!(table.lookup(&destination).unwrap().dev, host.dev);
        table.add_rule(Rule::for_local_ipv4_address(
            NetDeviceId::LOOPBACK,
            oldest.dev,
            Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 0), 24).unwrap(),
        ));
        assert_eq!(
            table.lookup(&destination).unwrap().dev,
            NetDeviceId::LOOPBACK
        );
    }
}
