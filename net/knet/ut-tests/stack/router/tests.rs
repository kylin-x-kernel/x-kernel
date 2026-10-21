// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, vec, vec::Vec};

use ktime_types::MonotonicInstant;
use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::{
    device::{LinkSendSnapshot, LinkSnapshot, LoopbackDevice},
    network::router::ROUTE_SCOPE_HOST,
};

fn valid_ipv4_cidr(address: Ipv4Address, prefix_len: u8) -> Ipv4Cidr {
    Ipv4Cidr::new(address, prefix_len).unwrap()
}

fn router_with_ready_loopback_packet() -> Router {
    let mut router = Router::new();
    let loopback = router.add_device(Box::new(LoopbackDevice::new())).unwrap();
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
        true
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
            .map(|packet| RxPoll::Consumed(Some(packet)))
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
            .add_device(Box::new(FailingRxDevice {
                link: LoopbackDevice::new(),
                packet: Some(tx_packet_buf(vec![0x45, 0, 0, 20])),
            }))
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
        let first = router.add_device(Box::new(LoopbackDevice::new())).unwrap();
        let second = router
            .add_device(Box::new(FailingRxDevice {
                link: LoopbackDevice::new(),
                packet: None,
            }))
            .unwrap();
        let primary = Ipv4AddrEntry {
            dev: first,
            addr: valid_ipv4_cidr(Ipv4Address::new(10, 0, 0, 1), 24),
            scope: ROUTE_SCOPE_UNIVERSE,
            broadcast: None,
        };
        router.add_ipv4_addr(primary).unwrap();
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
            router.add_ipv4_addr(connected).unwrap();
        }
        router.add_route_rule(configured).unwrap();
        if is_configured_first {
            router.add_ipv4_addr(connected).unwrap();
        }
        let expected_dev = if is_configured_first { second } else { first };
        let destination = Ipv4Address::new(192, 0, 2, 99).into();
        assert_eq!(router.table.lookup(&destination).unwrap().dev, expected_dev);

        let unrelated = Ipv4AddrEntry {
            addr: valid_ipv4_cidr(Ipv4Address::new(198, 51, 100, 7), 24),
            ..primary
        };
        router.add_ipv4_addr(unrelated).unwrap();
        assert_eq!(router.table.lookup(&destination).unwrap().dev, expected_dev);
        assert!(router.remove_ipv4_addr(unrelated));
        assert_eq!(router.table.lookup(&destination).unwrap().dev, expected_dev);

        router.replace_route_rule(configured, configured).unwrap();
        assert_eq!(router.table.lookup(&destination).unwrap().dev, first);
    }
}

#[def_test]
fn cross_device_address_iteration_does_not_reorder_surviving_routes() {
    let mut router = Router::new();
    let first = router.add_device(Box::new(LoopbackDevice::new())).unwrap();
    let second = router
        .add_device(Box::new(FailingRxDevice {
            link: LoopbackDevice::new(),
            packet: None,
        }))
        .unwrap();
    let first_primary = Ipv4AddrEntry {
        dev: first,
        addr: valid_ipv4_cidr(Ipv4Address::new(10, 0, 0, 1), 24),
        scope: ROUTE_SCOPE_UNIVERSE,
        broadcast: None,
    };
    let second_primary = Ipv4AddrEntry {
        dev: second,
        addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 1), 24),
        ..first_primary
    };
    let first_secondary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 2), 24),
        ..first_primary
    };
    for entry in [first_primary, second_primary, first_secondary] {
        router.add_ipv4_addr(entry).unwrap();
    }
    let destination = Ipv4Address::new(192, 0, 2, 99).into();
    assert_eq!(router.table.lookup(&destination).unwrap().dev, second);
    router.table.sync_ipv4_address_routes(
        &router.devices,
        &[first_primary, first_secondary, second_primary],
    );
    assert_eq!(router.table.lookup(&destination).unwrap().dev, second);
    assert_eq!(
        router
            .table
            .lookup(&destination)
            .unwrap()
            .preferred_source(),
        Some(second_primary.addr.address().into())
    );

    assert!(router.remove_ipv4_addr(second_primary));
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
    router.add_ipv4_addr(second_primary).unwrap();
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
fn connected_source_change_preserves_route_precedence() {
    let mut router = Router::new();
    let first = router.add_device(Box::new(LoopbackDevice::new())).unwrap();
    let second = router
        .add_device(Box::new(FailingRxDevice {
            link: LoopbackDevice::new(),
            packet: None,
        }))
        .unwrap();
    let primary = Ipv4AddrEntry {
        dev: first,
        addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 1), 24),
        scope: ROUTE_SCOPE_UNIVERSE,
        broadcast: None,
    };
    let competing = Ipv4AddrEntry {
        dev: second,
        addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 2), 24),
        ..primary
    };
    let secondary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 3), 24),
        ..primary
    };
    for entry in [primary, competing, secondary] {
        router.add_ipv4_addr(entry).unwrap();
    }
    router.remove_primary_ipv4_addr(first).unwrap();
    let destination = Ipv4Address::new(192, 0, 2, 99).into();
    let selected = router.table.lookup(&destination).unwrap();
    assert_eq!(selected.dev, first);
    assert_eq!(
        selected.preferred_source(),
        Some(secondary.addr.address().into())
    );
    assert_eq!(
        router.output_route_source(&destination, 0).unwrap(),
        secondary.addr.address().into()
    );

    assert_eq!(
        router
            .table
            .snapshot()
            .iter()
            .filter(|route| route.dev == first && route.filter == primary.addr.network().into())
            .count(),
        1
    );
    assert!(router.remove_ipv4_addr(secondary));
    assert_eq!(router.table.lookup(&destination).unwrap().dev, second);
    router.add_ipv4_addr(primary).unwrap();
    assert_eq!(router.table.lookup(&destination).unwrap().dev, second);
}

#[def_test]
fn ipv4_address_installs_local_and_connected_routes() {
    let mut router = Router::new();
    let dev = router.add_device(Box::new(LoopbackDevice::new())).unwrap();
    let entry = Ipv4AddrEntry {
        dev,
        addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
        scope: ROUTE_SCOPE_HOST,
        broadcast: None,
    };

    router.add_ipv4_addr(entry).unwrap();

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
fn ipv4_broadcast_detection_covers_limited_and_directed_addresses() {
    let mut router = Router::new();
    let dev = router.add_device(Box::new(LoopbackDevice::new())).unwrap();
    router
        .add_ipv4_addr(Ipv4AddrEntry {
            dev,
            addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
            scope: ROUTE_SCOPE_HOST,
            broadcast: None,
        })
        .unwrap();

    assert!(router.ipv4_dest_requires_broadcast(&IpAddress::Ipv4(Ipv4Address::BROADCAST)));
    assert!(
        router.ipv4_dest_requires_broadcast(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 255)))
    );
    assert!(!router.ipv4_dest_requires_broadcast(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 8))));
}

#[def_test]
fn ipv4_address_capacity_matches_smoltcp_interface_limit() {
    let mut router = Router::new();
    let dev = router.add_device(Box::new(LoopbackDevice::new())).unwrap();

    for index in 0..smoltcp::config::IFACE_MAX_ADDR_COUNT {
        router
            .add_ipv4_addr(Ipv4AddrEntry {
                dev,
                addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, index as u8 + 1), 32),
                scope: ROUTE_SCOPE_HOST,
                broadcast: None,
            })
            .unwrap();
    }

    assert_eq!(
        router.add_ipv4_addr(Ipv4AddrEntry {
            dev,
            addr: valid_ipv4_cidr(Ipv4Address::new(198, 51, 100, 1), 32),
            scope: ROUTE_SCOPE_HOST,
            broadcast: None,
        }),
        Err(LinuxError::ENOBUFS)
    );
}

#[def_test]
fn setting_primary_ipv4_address_keeps_secondary_addresses() {
    let mut router = Router::new();
    let dev = router.add_device(Box::new(LoopbackDevice::new())).unwrap();
    let primary = Ipv4AddrEntry {
        dev,
        addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
        scope: ROUTE_SCOPE_HOST,
        broadcast: Some(Ipv4Address::new(192, 0, 2, 111)),
    };
    let secondary = Ipv4AddrEntry {
        dev,
        addr: valid_ipv4_cidr(Ipv4Address::new(198, 51, 100, 7), 24),
        scope: ROUTE_SCOPE_HOST,
        broadcast: None,
    };
    router.add_ipv4_addr(primary).unwrap();
    router.add_ipv4_addr(secondary).unwrap();

    let replacement = valid_ipv4_cidr(Ipv4Address::new(203, 0, 113, 7), 24);
    router.set_primary_ipv4_addr(dev, replacement).unwrap();

    assert_eq!(router.ipv4_addr_entries().len(), 2);
    assert_eq!(router.ipv4_addr_entries()[0].addr, replacement);
    assert_eq!(router.ipv4_addr_entries()[0].scope, ROUTE_SCOPE_UNIVERSE);
    assert_eq!(router.ipv4_addr_entries()[0].broadcast, None);
    assert_eq!(router.ipv4_addr_entries()[1], secondary);
}

#[def_test]
fn removing_primary_ipv4_address_keeps_secondary_addresses() {
    let mut router = Router::new();
    let dev = router.add_device(Box::new(LoopbackDevice::new())).unwrap();
    let primary = Ipv4AddrEntry {
        dev,
        addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
        scope: ROUTE_SCOPE_HOST,
        broadcast: None,
    };
    let secondary = Ipv4AddrEntry {
        dev,
        addr: valid_ipv4_cidr(Ipv4Address::new(198, 51, 100, 7), 24),
        scope: ROUTE_SCOPE_HOST,
        broadcast: None,
    };
    router.add_ipv4_addr(primary).unwrap();
    router.add_ipv4_addr(secondary).unwrap();

    router.remove_primary_ipv4_addr(dev).unwrap();

    assert_eq!(router.ipv4_addr_entries(), &[secondary]);
}

#[def_test]
fn setting_ipv4_netmask_keeps_secondary_scope_and_custom_broadcast() {
    let mut router = Router::new();
    let dev = router.add_device(Box::new(LoopbackDevice::new())).unwrap();
    let custom_broadcast = Ipv4Address::new(192, 0, 2, 111);
    let secondary = Ipv4AddrEntry {
        dev,
        addr: valid_ipv4_cidr(Ipv4Address::new(198, 51, 100, 7), 24),
        scope: ROUTE_SCOPE_UNIVERSE,
        broadcast: None,
    };
    router
        .add_ipv4_addr(Ipv4AddrEntry {
            dev,
            addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
            scope: ROUTE_SCOPE_HOST,
            broadcast: Some(custom_broadcast),
        })
        .unwrap();
    router.add_ipv4_addr(secondary).unwrap();

    router.set_ipv4_netmask(dev, 16).unwrap();

    assert_eq!(router.ipv4_addr_entries().len(), 2);
    assert_eq!(router.ipv4_addr_entries()[0].addr.prefix_len(), 16);
    assert_eq!(router.ipv4_addr_entries()[0].scope, ROUTE_SCOPE_HOST);
    assert_eq!(
        router.ipv4_addr_entries()[0].broadcast,
        Some(custom_broadcast)
    );
    assert_eq!(router.ipv4_addr_entries()[1], secondary);
}

#[def_test]
fn setting_ipv4_netmask_collision_keeps_existing_target() {
    let mut router = Router::new();
    let dev = router.add_device(Box::new(LoopbackDevice::new())).unwrap();
    let primary = Ipv4AddrEntry {
        dev,
        addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
        scope: ROUTE_SCOPE_HOST,
        broadcast: None,
    };
    let secondary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(primary.addr.address(), 16),
        broadcast: Some(Ipv4Address::new(192, 0, 255, 111)),
        ..primary
    };
    router.add_ipv4_addr(primary).unwrap();
    router.add_ipv4_addr(secondary).unwrap();
    assert_eq!(router.set_ipv4_netmask(dev, 24), Ok(()));
    assert_eq!(router.ipv4_addr_entries(), &[primary, secondary]);
    assert_eq!(router.set_ipv4_netmask(dev, 16), Ok(()));
    assert_eq!(router.ipv4_addr_entries(), &[secondary]);
    assert_eq!(router.local_ipv4_addrs(), vec![secondary.addr]);
    let destination = Ipv4Address::new(192, 0, 2, 8).into();
    assert_eq!(
        router.table.lookup(&destination).unwrap().filter,
        secondary.addr.network().into()
    );
    assert_eq!(router.output_route_source(&destination, 0), Ok(primary.addr.address().into()));
}

#[def_test]
fn deleting_ipv4_address_removes_automatic_routes() {
    let mut router = Router::new();
    let dev = router.add_device(Box::new(LoopbackDevice::new())).unwrap();
    let entry = Ipv4AddrEntry {
        dev,
        addr: valid_ipv4_cidr(Ipv4Address::new(198, 51, 100, 7), 24),
        scope: ROUTE_SCOPE_HOST,
        broadcast: None,
    };
    let secondary = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(entry.addr.address(), 16),
        ..entry
    };
    router.add_ipv4_addr(entry).unwrap();
    router.add_ipv4_addr(secondary).unwrap();
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

    assert!(router.remove_ipv4_addr(entry));
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
    assert!(router.remove_ipv4_addr(secondary));
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
    let dev = router.add_device(Box::new(LoopbackDevice::new())).unwrap();
    router
        .add_ipv4_addr(Ipv4AddrEntry {
            dev,
            addr: valid_ipv4_cidr(Ipv4Address::new(192, 0, 2, 7), 24),
            scope: ROUTE_SCOPE_HOST,
            broadcast: None,
        })
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
