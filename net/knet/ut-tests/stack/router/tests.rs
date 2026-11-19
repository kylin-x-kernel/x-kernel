// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{vec, vec::Vec};

use ktime_types::MonotonicInstant;
use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::{
    device::{LinkSendSnapshot, LinkSnapshot, LoopbackDevice},
    network::router::{
        ROUTE_PROTOCOL_KERNEL, ROUTE_SCOPE_HOST, ROUTE_SCOPE_LINK, ROUTE_TABLE_LOCAL,
        ROUTE_TYPE_BROADCAST,
    },
};

fn valid_ipv4_cidr(address: Ipv4Address, prefix_len: u8) -> Ipv4Cidr {
    Ipv4Cidr::new(address, prefix_len).unwrap()
}

fn router_with_ready_loopback_packet() -> Router {
    let mut router = Router::new();
    let loopback = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    let packet = PacketBuf::from_ip_packet_vec(1, vec![0x45, 0, 0, 20], PacketOwner::Ipv4Stack);
    let _ = router
        .devices
        .get_mut_by_id(loopback)
        .unwrap()
        .send_ip_packet(
            1,
            IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1)),
            IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1)),
            packet,
            MonotonicInstant::ORIGIN,
        );
    router
}

struct FailingRxDevice {
    link: LoopbackDevice,
    packet: Option<PacketBuf>,
    up: bool,
}

impl NetDevice for FailingRxDevice {
    fn name(&self) -> &str {
        self.link.name()
    }

    fn link_kind(&self) -> LinkKind {
        LinkKind::Ethernet
    }

    fn mtu(&self) -> usize {
        self.link.mtu()
    }

    fn is_link_up(&self) -> bool {
        self.up
    }

    fn set_link_up(&mut self, is_up: bool) {
        self.up = is_up;
    }

    fn link_snapshot(&self, ifindex: i32) -> LinkSnapshot {
        self.link.link_snapshot(ifindex)
    }

    fn link_send_snapshot(&self) -> LinkSendSnapshot {
        self.link.link_send_snapshot()
    }

    fn poll_rx(&mut self, _ifindex: i32, _timestamp: MonotonicInstant) -> KResult<RxPoll> {
        self.packet
            .take()
            .map(|packet| RxPoll::Consumed(Some(packet.into())))
            .ok_or(KError::Io)
    }

    fn has_rx_work(&self) -> bool {
        self.packet.is_some()
    }

    fn send_ip_packet(
        &mut self,
        _ifindex: i32,
        _next_hop: IpAddress,
        _source_addr: IpAddress,
        _packet: PacketBuf,
        _timestamp: MonotonicInstant,
    ) -> bool {
        false
    }

    fn register_rx_waker(
        &self,
        source_waker: &core::task::Waker,
        context: &mut PollContext<'_>,
    ) -> Result<(), PollRegisterError> {
        self.link.register_rx_waker(source_waker, context)
    }
}

#[def_test]
fn rx_failure_preserves_partial_batch_and_advances_device_cursor() {
    let mut router = Router::new();
    for _ in 0..2 {
        router
            .add_device(kalloc_types::KBox::new(FailingRxDevice {
                link: LoopbackDevice::new(),
                packet: Some(tx_packet_buf(vec![0x45, 0, 0, 20])),
                up: true,
            }).unwrap())
            .unwrap();
    }
    let mut packets = Vec::new();
    let first = router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 4, &mut packets);
    assert_eq!(first.work_done, 1);
    assert_eq!(packets.len(), 1);
    assert_eq!(first.error, Some((2, KError::Io)));
    assert!(first.has_more);

    let second = router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 4, &mut packets);
    assert_eq!(second.work_done, 1);
    assert_eq!(packets.len(), 1);
    assert_eq!(second.error, Some((3, KError::Io)));
    assert!(!second.has_more);
}

#[def_test(serial)]
fn exact_rx_budget_does_not_report_empty_device() {
    let mut router = router_with_ready_loopback_packet();
    let mut packets = Vec::new();

    let drain = router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 1, &mut packets);

    assert_eq!(drain.work_done, 1);
    assert_eq!(drain.error, None);
    assert!(!drain.has_more);
}

#[def_test(serial)]
fn zero_rx_budget_reports_ready_device() {
    let mut router = router_with_ready_loopback_packet();
    let mut packets = Vec::new();

    let drain = router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 0, &mut packets);

    assert_eq!(drain.work_done, 0);
    assert!(drain.has_more);
}

#[def_test]
fn control_dispatch_does_not_free_data_tx_capacity() {
    let mut router = Router::new();
    router
        .control_tx_queue
        .push_back(tx_packet_buf(vec![0; 20]));
    let capacity_before = router.available_tx_packet_slots();

    let (work_done, _) = router.dispatch_budgeted(MonotonicInstant::ORIGIN, 1);

    assert_eq!(work_done, 1);
    assert_eq!(router.available_tx_packet_slots(), capacity_before);
}

#[def_test]
fn data_dispatch_frees_data_tx_capacity() {
    let mut router = Router::new();
    assert!(
        router
            .data_tx_queue
            .try_push(tx_packet_buf(vec![0; 20]))
            .is_ok()
    );
    let capacity_before = router.available_tx_packet_slots();

    let (work_done, _) = router.dispatch_budgeted(MonotonicInstant::ORIGIN, 1);

    assert_eq!(work_done, 1);
    assert_eq!(router.available_tx_packet_slots(), capacity_before + 1);
}

#[def_test]
fn unrelated_address_update_preserves_automatic_and_configured_route_order() {
    for is_configured_first in [false, true] {
        let mut router = Router::new();
        let first = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
        let second = router
            .add_device(kalloc_types::KBox::new(FailingRxDevice {
                link: LoopbackDevice::new(),
                packet: None,
                up: true,
            }).unwrap())
            .unwrap();
        let primary = Ipv4AddrEntry {
            addr: valid_ipv4_cidr(Ipv4Address::new(10, 0, 0, 1), 24),
            scope: ROUTE_SCOPE_UNIVERSE,
            broadcast: None,
            no_prefix_route: false,
            label: None,
        };
        router.add_ipv4_addr(first, primary).unwrap();
        let connected = Ipv4AddrEntry {
            addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
            ..primary
        };
        let configured = Rule::new(
            connected.addr.network().into(),
            None,
            second,
            primary.addr.address().into(),
        );
        if !is_configured_first {
            router.add_ipv4_addr(first, connected).unwrap();
        }
        router.add_route_rule(configured).unwrap();
        if is_configured_first {
            router.add_ipv4_addr(first, connected).unwrap();
        }
        let expected_dev = if is_configured_first { second } else { first };
        let destination = Ipv4Address::new(192, 0, 2, 99).into();
        assert_eq!(router.table.lookup(&destination).unwrap().dev, expected_dev);

        let unrelated = Ipv4AddrEntry {
            addr: valid_ipv4_cidr(Ipv4Address::new(198, 51, 100, 7), 24),
            ..primary
        };
        router.add_ipv4_addr(first, unrelated).unwrap();
        assert_eq!(router.table.lookup(&destination).unwrap().dev, expected_dev);
        assert!(!router.remove_ipv4_addr(first, unrelated.addr).is_empty());
        assert_eq!(router.table.lookup(&destination).unwrap().dev, expected_dev);

        router.replace_route_rule(configured, configured).unwrap();
        assert_eq!(router.table.lookup(&destination).unwrap().dev, first);
    }
}

#[def_test]
fn cross_device_address_iteration_does_not_reorder_surviving_routes() {
    let mut router = Router::new();
    let first = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    let second = router
        .add_device(kalloc_types::KBox::new(FailingRxDevice {
            link: LoopbackDevice::new(),
            packet: None,
            up: true,
        }).unwrap())
        .unwrap();
    let first_primary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(10, 0, 0, 1), 24),
        scope: ROUTE_SCOPE_UNIVERSE,
        broadcast: None,
        no_prefix_route: false,
        label: None,
    };
    let second_primary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 1), 24),
        ..first_primary
    };
    let first_secondary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 2), 24),
        ..first_primary
    };
    for (dev, entry) in [
        (first, first_primary),
        (second, second_primary),
        (first, first_secondary),
    ] {
        router.add_ipv4_addr(dev, entry).unwrap();
    }
    let destination = Ipv4Address::new(192, 0, 2, 99).into();
    assert_eq!(router.table.lookup(&destination).unwrap().dev, second);
    assert_eq!(
        router.local_ipv4_addrs(),
        vec![
            first_primary.addr,
            first_secondary.addr,
            second_primary.addr
        ]
    );
    router
        .set_primary_ipv4_addr(first, valid_ipv4_cidr(Ipv4Address::new(10, 1, 0, 1), 24))
        .unwrap();
    assert_eq!(router.table.lookup(&destination).unwrap().dev, second);
    assert_eq!(
        router
            .table
            .lookup(&destination)
            .unwrap()
            .preferred_source(),
        Some(second_primary.addr.address().into())
    );

    assert!(
        !router
            .remove_ipv4_addr(second, second_primary.addr)
            .is_empty()
    );
    assert_eq!(router.table.lookup(&destination).unwrap().dev, first);
    assert!(
        router
            .table
            .lookup(&second_primary.addr.address().into())
            .unwrap()
            .filter
            .prefix_len()
            < 32
    );
    router.add_ipv4_addr(second, second_primary).unwrap();
    assert_eq!(router.table.lookup(&destination).unwrap().dev, first);
    assert_eq!(
        router
            .table
            .lookup(&second_primary.addr.address().into())
            .unwrap()
            .filter
            .prefix_len(),
        32
    );
}

#[def_test]
fn deleting_connected_primary_cascades_secondaries_and_falls_back() {
    let mut router = Router::new();
    let first = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    let second = router
        .add_device(kalloc_types::KBox::new(FailingRxDevice {
            link: LoopbackDevice::new(),
            packet: None,
            up: true,
        }).unwrap())
        .unwrap();
    let primary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 1), 24),
        scope: ROUTE_SCOPE_UNIVERSE,
        broadcast: None,
        no_prefix_route: false,
        label: None,
    };
    let competing = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 2), 24),
        ..primary
    };
    let secondary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 3), 24),
        ..primary
    };
    for (dev, entry) in [(first, primary), (second, competing), (first, secondary)] {
        router.add_ipv4_addr(dev, entry).unwrap();
    }
    // Removing the primary also removes its matching secondary.  The other
    // device's primary for the same subnet stays and provides the source.
    router.remove_primary_ipv4_addr(first).unwrap();
    assert!(router.ipv4_addr_entries(first).is_empty());
    let destination = Ipv4Address::new(192, 0, 2, 99).into();
    let selected = router.table.lookup(&destination).unwrap();
    assert_eq!(selected.dev, second);
    assert_eq!(
        selected.preferred_source(),
        Some(competing.addr.address().into())
    );
    assert_eq!(
        router.output_route_source(&destination, 0).unwrap(),
        competing.addr.address().into()
    );
    assert!(router.remove_ipv4_addr(first, secondary.addr).is_empty());
    assert_eq!(router.table.lookup(&destination).unwrap().dev, second);
}

#[def_test]
fn local_address_output_survives_owner_link_down_without_oif() {
    let mut router = Router::new();
    let loopback = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    let owner = router
        .add_device(kalloc_types::KBox::new(FailingRxDevice {
            link: LoopbackDevice::new(),
            packet: None,
            up: true,
        }).unwrap())
        .unwrap();
    let entry = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(198, 51, 100, 7), 24),
        scope: ROUTE_SCOPE_UNIVERSE,
        broadcast: None,
        no_prefix_route: false,
        label: None,
    };
    router.add_ipv4_addr(owner, entry).unwrap();
    for (source, output, expected) in [
        (None, None, Ipv4Address::LOOPBACK),
        (Some(entry.addr.address()), None, entry.addr.address()),
        (None, Some(owner), entry.addr.address()),
    ] {
        let route = router
            .lookup_ipv4_output(Ipv4Address::UNSPECIFIED, source, output)
            .unwrap();
        assert_eq!(route.destination, expected);
        assert_eq!(route.source, Some(expected));
        assert_eq!(route.oif, loopback);
    }
    router
        .devices
        .get_mut_by_id(owner)
        .unwrap()
        .set_link_up(false);
    router.sync_ipv4_address_routes();

    let destination = entry.addr.address().into();
    let local = router.table.lookup(&destination).unwrap();
    assert_eq!(local.dev, loopback);
    assert_eq!(local.lookup_device(), owner);
    // Linux keeps the local-table route and resolves output through loopback
    // even after the address owner goes down.
    assert_eq!(
        router.output_route_source(&destination, 0),
        Ok(entry.addr.address().into())
    );
    // An explicit OIF still validates the FIB device before the lookup.
    assert_eq!(
        router.output_route_source(&destination, owner.ifindex()),
        Err(LinuxError::ENETUNREACH.into())
    );
}

#[def_test]
fn ipv4_address_installs_local_and_connected_routes() {
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    let entry = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
        scope: ROUTE_SCOPE_HOST,
        broadcast: None,
        no_prefix_route: false,
        label: None,
    };

    router.add_ipv4_addr(dev, entry).unwrap();

    let local = router
        .table
        .lookup(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 7)))
        .unwrap();
    assert_eq!(local.filter.prefix_len(), 32);
    assert_eq!(local.dev, dev);
    assert_eq!(local.scope, ROUTE_SCOPE_HOST);
    assert_eq!(local.protocol, ROUTE_PROTOCOL_KERNEL);

    let connected = router
        .table
        .lookup(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 8)))
        .unwrap();
    assert_eq!(connected.filter.prefix_len(), 24);
    assert_eq!(connected.dev, dev);
    assert_eq!(connected.scope, ROUTE_SCOPE_HOST);
    assert_eq!(connected.protocol, ROUTE_PROTOCOL_KERNEL);
}

#[def_test]
fn explicit_ipv4_broadcast_installs_and_replaces_a_broadcast_route() {
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    let assigned = Ipv4Address::new(192, 0, 2, 111);
    let entry = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
        scope: ROUTE_SCOPE_UNIVERSE,
        broadcast: Some(assigned),
        no_prefix_route: false,
        label: None,
    };
    router.add_ipv4_addr(dev, entry).unwrap();

    let explicit = router
        .table
        .snapshot()
        .into_iter()
        .find(|route| route.filter == valid_ipv4_cidr(assigned, 32).into())
        .unwrap();
    assert_eq!(explicit.dev, dev);
    assert_eq!(explicit.table, ROUTE_TABLE_LOCAL);
    assert_eq!(explicit.route_type, ROUTE_TYPE_BROADCAST);
    assert_eq!(explicit.scope, ROUTE_SCOPE_LINK);
    assert_eq!(explicit.protocol, ROUTE_PROTOCOL_KERNEL);
    assert_eq!(
        explicit.preferred_source(),
        Some(IpAddress::Ipv4(entry.addr.address()))
    );

    // The computed network broadcast coexists with an explicit assignment.
    let computed = Ipv4Address::new(192, 0, 2, 255);
    assert!(
        router
            .table
            .snapshot()
            .iter()
            .any(|route| route.filter == valid_ipv4_cidr(computed, 32).into())
    );

    let replacement = Ipv4Address::new(192, 0, 2, 222);
    router.set_ipv4_broadcast(dev, replacement).unwrap();
    let routes = router.table.snapshot();
    assert!(
        !routes
            .iter()
            .any(|route| route.filter == valid_ipv4_cidr(assigned, 32).into())
    );
    assert!(
        routes
            .iter()
            .any(|route| route.filter == valid_ipv4_cidr(replacement, 32).into())
    );
    assert!(
        routes
            .iter()
            .any(|route| route.filter == valid_ipv4_cidr(computed, 32).into())
    );

    // Assigning the limited broadcast suppresses only the explicit route.
    router
        .set_ipv4_broadcast(dev, Ipv4Address::BROADCAST)
        .unwrap();
    let routes = router.table.snapshot();
    assert!(
        !routes
            .iter()
            .any(|route| route.filter == valid_ipv4_cidr(replacement, 32).into())
    );
    assert!(
        routes
            .iter()
            .any(|route| route.filter == valid_ipv4_cidr(computed, 32).into())
    );
}

#[def_test]
fn implicit_ipv4_broadcast_assignment_updates_stored_value_without_duplicate_route() {
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    router
        .add_ipv4_addr(
            dev,
            Ipv4AddrEntry {
                addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
                scope: ROUTE_SCOPE_UNIVERSE,
                broadcast: None,
                no_prefix_route: false,
                label: None,
            },
        )
        .unwrap();

    let computed = Ipv4Address::new(192, 0, 2, 255);
    router.set_ipv4_broadcast(dev, computed).unwrap();
    assert_eq!(router.ipv4_addr_entries(dev)[0].broadcast, Some(computed));
    assert_eq!(
        router
            .table
            .snapshot()
            .iter()
            .filter(|route| route.filter == valid_ipv4_cidr(computed, 32).into())
            .count(),
        1
    );

    router
        .set_ipv4_broadcast(dev, Ipv4Address::UNSPECIFIED)
        .unwrap();
    assert_eq!(router.ipv4_addr_entries(dev)[0].broadcast, None);
    assert_eq!(
        router
            .table
            .snapshot()
            .iter()
            .filter(|route| route.filter == valid_ipv4_cidr(computed, 32).into())
            .count(),
        1
    );
}

#[def_test]
fn zero_ipv4_broadcast_is_not_derived_by_netmask_changes() {
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    router
        .add_ipv4_addr(
            dev,
            Ipv4AddrEntry {
                addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
                scope: ROUTE_SCOPE_UNIVERSE,
                broadcast: None,
                no_prefix_route: false,
                label: None,
            },
        )
        .unwrap();

    router.set_ipv4_netmask(dev, 16).unwrap();
    assert_eq!(router.ipv4_addr_entries(dev)[0].broadcast, None);
}

#[def_test]
fn explicit_ipv4_broadcast_route_follows_link_state_and_prefix_suppression() {
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    let assigned = Ipv4Address::new(198, 51, 100, 111);
    let entry = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(198, 51, 100, 7), 24),
        scope: ROUTE_SCOPE_UNIVERSE,
        broadcast: Some(assigned),
        no_prefix_route: true,
        label: None,
    };
    router.add_ipv4_addr(dev, entry).unwrap();
    let broadcast_route = valid_ipv4_cidr(assigned, 32).into();
    let connected_route = entry.addr.network().into();
    assert!(
        router
            .table
            .snapshot()
            .iter()
            .any(|route| route.filter == broadcast_route)
    );
    assert!(
        !router
            .table
            .snapshot()
            .iter()
            .any(|route| route.filter == connected_route)
    );

    router
        .devices
        .get_mut_by_id(dev)
        .unwrap()
        .set_link_up(false);
    router.sync_ipv4_address_routes();
    assert!(
        !router
            .table
            .snapshot()
            .iter()
            .any(|route| route.filter == broadcast_route)
    );

    router.devices.get_mut_by_id(dev).unwrap().set_link_up(true);
    router.sync_ipv4_address_routes();
    assert!(
        router
            .table
            .snapshot()
            .iter()
            .any(|route| route.filter == broadcast_route)
    );
    assert!(
        !router
            .table
            .snapshot()
            .iter()
            .any(|route| route.filter == connected_route)
    );
}

#[def_test]
fn ipv4_netmask_change_updates_computed_broadcast_and_keeps_explicit_one() {
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    let explicit = Ipv4Address::new(192, 168, 1, 111);
    router
        .add_ipv4_addr(
            dev,
            Ipv4AddrEntry {
                addr: valid_ipv4_cidr(Ipv4Address::new(192, 168, 1, 7), 24),
                scope: ROUTE_SCOPE_UNIVERSE,
                broadcast: Some(explicit),
                no_prefix_route: false,
                label: None,
            },
        )
        .unwrap();
    router.set_ipv4_netmask(dev, 16).unwrap();
    let routes = router.table.snapshot();
    assert!(
        routes
            .iter()
            .any(|route| route.filter == valid_ipv4_cidr(explicit, 32).into())
    );
    assert!(
        routes.iter().any(|route| route.filter
            == valid_ipv4_cidr(Ipv4Address::new(192, 168, 255, 255), 32).into())
    );
    assert!(!routes.iter().any(
        |route| route.filter == valid_ipv4_cidr(Ipv4Address::new(192, 168, 1, 255), 32).into()
    ));
}

#[def_test]
fn ipv4_broadcast_detection_covers_limited_and_directed_addresses() {
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    router
        .add_ipv4_addr(
            dev,
            Ipv4AddrEntry {
                addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
                scope: ROUTE_SCOPE_HOST,
                broadcast: None,
                no_prefix_route: false,
                label: None,
            },
        )
        .unwrap();

    assert!(router.ipv4_dest_requires_broadcast(&IpAddress::Ipv4(Ipv4Address::BROADCAST)));
    assert!(
        router.ipv4_dest_requires_broadcast(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 255)))
    );
    assert!(!router.ipv4_dest_requires_broadcast(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 8))));
}

#[def_test]
fn setting_primary_ipv4_address_preserves_unrelated_addresses() {
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    let primary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
        scope: ROUTE_SCOPE_HOST,
        broadcast: Some(Ipv4Address::new(192, 0, 2, 111)),
        no_prefix_route: true,
        label: None,
    };
    let secondary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(198, 51, 100, 7), 24),
        scope: ROUTE_SCOPE_HOST,
        broadcast: None,
        no_prefix_route: false,
        label: None,
    };
    router.add_ipv4_addr(dev, primary).unwrap();
    router.add_ipv4_addr(dev, secondary).unwrap();

    router
        .set_primary_ipv4_addr(dev, valid_ipv4_cidr(primary.addr.address(), 16))
        .unwrap();
    assert_eq!(router.ipv4_addr_entries(dev)[0], primary);
    assert_eq!(
        router.set_primary_ipv4_addr(dev, secondary.addr),
        Err(LinuxError::EEXIST)
    );
    assert_eq!(router.ipv4_addr_entries(dev), &[primary, secondary]);

    let replacement = valid_ipv4_cidr(Ipv4Address::new(203, 0, 113, 7), 24);
    router.set_primary_ipv4_addr(dev, replacement).unwrap();

    assert_eq!(router.ipv4_addr_entries(dev).len(), 2);
    // Linux reinserts a replacement at its scope-derived position.  The new
    // universe-scope primary follows the surviving host-scope address.
    assert_eq!(router.ipv4_addr_entries(dev)[0], secondary);
    assert_eq!(router.ipv4_addr_entries(dev)[1].addr, replacement);
    assert_eq!(router.ipv4_addr_entries(dev)[1].scope, ROUTE_SCOPE_UNIVERSE);
    assert_eq!(router.ipv4_addr_entries(dev)[1].broadcast, None);
    // `SIOCSIFADDR` clears the derived broadcast and resets scope, but it
    // preserves `IFA_F_NOPREFIXROUTE` on the replaced address.
    assert!(router.ipv4_addr_entries(dev)[1].no_prefix_route);
}

#[def_test]
fn removing_primary_ipv4_address_preserves_unrelated_addresses() {
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    let primary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
        scope: ROUTE_SCOPE_HOST,
        broadcast: None,
        no_prefix_route: false,
        label: None,
    };
    let secondary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(198, 51, 100, 7), 24),
        scope: ROUTE_SCOPE_HOST,
        broadcast: None,
        no_prefix_route: false,
        label: None,
    };
    router.add_ipv4_addr(dev, primary).unwrap();
    router.add_ipv4_addr(dev, secondary).unwrap();

    router.remove_primary_ipv4_addr(dev).unwrap();

    assert_eq!(router.ipv4_addr_entries(dev), &[secondary]);
    assert_eq!(router.interface_ipv4_addr("lo"), Ok(secondary.addr));
}

#[def_test]
fn setting_ipv4_netmask_keeps_secondary_scope_and_custom_broadcast() {
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    let custom_broadcast = Ipv4Address::new(192, 0, 2, 111);
    let secondary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(198, 51, 100, 7), 24),
        scope: ROUTE_SCOPE_UNIVERSE,
        broadcast: None,
        no_prefix_route: false,
        label: None,
    };
    router
        .add_ipv4_addr(
            dev,
            Ipv4AddrEntry {
                addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
                scope: ROUTE_SCOPE_HOST,
                broadcast: Some(custom_broadcast),
                no_prefix_route: false,
                label: None,
            },
        )
        .unwrap();
    router.add_ipv4_addr(dev, secondary).unwrap();

    router.set_ipv4_netmask(dev, 16).unwrap();

    assert_eq!(router.ipv4_addr_entries(dev).len(), 2);
    assert_eq!(router.ipv4_addr_entries(dev)[0].addr.prefix_len(), 16);
    assert_eq!(router.ipv4_addr_entries(dev)[0].scope, ROUTE_SCOPE_HOST);
    assert_eq!(
        router.ipv4_addr_entries(dev)[0].broadcast,
        Some(custom_broadcast)
    );
    assert_eq!(router.ipv4_addr_entries(dev)[1], secondary);
}

#[def_test]
fn setting_ipv4_netmask_collision_keeps_existing_target() {
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    let primary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
        scope: ROUTE_SCOPE_HOST,
        broadcast: None,
        no_prefix_route: false,
        label: None,
    };
    let secondary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(primary.addr.address(), 16),
        broadcast: Some(Ipv4Address::new(192, 0, 255, 111)),
        no_prefix_route: false,
        label: None,
        ..primary
    };
    router.add_ipv4_addr(dev, primary).unwrap();
    router.add_ipv4_addr(dev, secondary).unwrap();
    assert_eq!(router.set_ipv4_netmask(dev, 24), Ok(()));
    assert_eq!(router.ipv4_addr_entries(dev), &[primary, secondary]);
    assert_eq!(router.set_ipv4_netmask(dev, 16), Ok(()));
    assert_eq!(router.ipv4_addr_entries(dev), &[secondary]);
    assert_eq!(router.local_ipv4_addrs(), vec![secondary.addr]);
    let destination = Ipv4Address::new(192, 0, 2, 8).into();
    assert_eq!(
        router.table.lookup(&destination).unwrap().filter,
        secondary.addr.network().into()
    );
    assert_eq!(
        router.output_route_source(&destination, 0),
        Ok(primary.addr.address().into())
    );
}

#[def_test]
fn deleting_ipv4_address_removes_automatic_routes() {
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    let entry = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(198, 51, 100, 7), 24),
        scope: ROUTE_SCOPE_HOST,
        broadcast: None,
        no_prefix_route: false,
        label: None,
    };
    let secondary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(entry.addr.address(), 16),
        ..entry
    };
    router.add_ipv4_addr(dev, entry).unwrap();
    router.add_ipv4_addr(dev, secondary).unwrap();
    let host = valid_ipv4_cidr(entry.addr.address(), 32);
    assert_eq!(
        router
            .table
            .snapshot()
            .iter()
            .filter(|route| route.filter == host.into())
            .count(),
        1
    );

    assert!(!router.remove_ipv4_addr(dev, entry.addr).is_empty());
    assert_eq!(
        router.table.lookup(&host.address().into()).unwrap().filter,
        host.into()
    );
    assert!(
        router
            .table
            .lookup(&Ipv4Address::new(198, 51, 100, 8).into())
            .is_some()
    );
    assert!(!router.remove_ipv4_addr(dev, secondary.addr).is_empty());
    assert!(router.table.lookup(&host.address().into()).is_none());
    assert!(
        router
            .table
            .lookup(&Ipv4Address::new(198, 51, 100, 8).into())
            .is_none()
    );
}

#[def_test]
fn configured_gateway_must_be_on_device_subnet() {
    let mut router = Router::new();
    let dev = router.add_device(kalloc_types::KBox::new(LoopbackDevice::new()).unwrap()).unwrap();
    router
        .add_ipv4_addr(
            dev,
            Ipv4AddrEntry {
                addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
                scope: ROUTE_SCOPE_HOST,
                broadcast: None,
                no_prefix_route: false,
                label: None,
            },
        )
        .unwrap();

    let unreachable = Rule::new(
        valid_ipv4_cidr(Ipv4Address::UNSPECIFIED, 0).into(),
        Some(IpAddress::Ipv4(Ipv4Address::new(198, 51, 100, 1))),
        dev,
        IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 7)),
    );
    assert_eq!(
        router.add_route_rule(unreachable),
        Err(LinuxError::ENETUNREACH)
    );

    let default = Rule::new(
        valid_ipv4_cidr(Ipv4Address::UNSPECIFIED, 0).into(),
        Some(IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 1))),
        dev,
        IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 7)),
    );
    router.add_route_rule(default).unwrap();
    let routes_before = router.table.snapshot().len();
    assert_eq!(
        router.replace_route_rule(default, unreachable),
        Err(LinuxError::ENETUNREACH)
    );
    assert_eq!(router.table.snapshot().len(), routes_before);
    assert_eq!(
        router
            .table
            .lookup(&Ipv4Address::new(203, 0, 113, 1).into())
            .unwrap()
            .via,
        default.via
    );
    router.table.remove_exact_rule(default);
    assert!(
        router
            .table
            .snapshot()
            .iter()
            .all(|route| !route.is_configured())
    );
}

#[def_test]
fn forwarding_route_preserves_a_remote_source_and_selects_the_gateway() {
    let mut router = Router::new();
    let dev = router
        .add_device(
            KBox::new(FailingRxDevice {
                link: LoopbackDevice::new(),
                packet: None,
                up: true,
            })
            .unwrap(),
        )
        .unwrap();
    router
        .add_ipv4_addr(
            dev,
            Ipv4AddrEntry {
                addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 1), 24),
                scope: ROUTE_SCOPE_UNIVERSE,
                broadcast: None,
                no_prefix_route: false,
                label: None,
            },
        )
        .unwrap();
    let destination = Ipv4Address::new(203, 0, 113, 9);
    let gateway = Ipv4Address::new(192, 0, 2, 254);
    router
        .add_route_rule(Rule::new(
            valid_ipv4_cidr(Ipv4Address::new(203, 0, 113, 0), 24).into(),
            Some(gateway.into()),
            dev,
            Ipv4Address::new(192, 0, 2, 1).into(),
        ))
        .unwrap();
    let source = Ipv4Address::new(198, 51, 100, 7);
    let bytes =
        ipv4::build_ipv4_packet(source, destination, ipv4::PROTOCOL_UDP, 64, &[0; 8]).unwrap();
    let original = PacketBuf::from_ip_packet_vec(7, bytes, PacketOwner::DeviceRx);
    let forward_input = |mut packet| {
        ipv4::validate_input_packet(&mut packet).unwrap();
        Ipv4ForwardInput::from_validated(packet).unwrap()
    };
    let (prepared, route) = router
        .prepare_ipv4_forward(forward_input(original.clone()))
        .unwrap();
    assert_eq!(route.device, dev);
    assert_eq!(route.next_hop, gateway);
    let header = Ipv4Header::parse_input(prepared.packet().network_packet().unwrap()).unwrap();
    assert_eq!(header.src_addr(), source);
    assert_eq!(header.bytes()[8], 63);
}
