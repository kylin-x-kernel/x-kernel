// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use alloc::{boxed::Box, format, string::String, vec, vec::Vec};
use core::{sync::atomic::Ordering, task::Waker};

use kcred::Cred;
use kerrno::LinuxError;
use kio::Cursor;
use ktime_types::MonotonicInstant;
use smoltcp::wire::{IpAddress, IpCidr, Ipv4Address};
use unittest::def_test;

use super::{
    rtnetlink::*,
    socket::publish_kobject_uevent,
    wire::{
        IfAddrMsg, IfInfoMsg, NlMsgHeader, addr as wire_addr, build_nlmsg, link as wire_link,
        neigh as wire_neigh, parse_attrs, push_attr, push_i32_ne, push_u8, push_u16_ne,
        push_u32_ne, read_i32_ne, read_u16_ne, route as wire_route,
    },
    *,
};
use crate::{
    RecvOptions, SERVICE, SendOptions, SocketAddrEx,
    buf::PacketBuf,
    device::{
        IF_OPER_DOWN, IF_OPER_UNKNOWN, IF_OPER_UP, LINK_FLAG_BROADCAST, LINK_FLAG_LOOPBACK,
        LINK_FLAG_LOWER_UP, LINK_FLAG_MULTICAST, LINK_FLAG_RUNNING, LINK_FLAG_UP, LinkKind,
        LinkSendSnapshot, LinkSnapshot, NeighborUpdate, NetDevice,
    },
    router::{Ipv4AddrEntry, Router, Rule},
    service::Service,
    socket::SocketOps,
};

struct TestDevice {
    link: LinkSnapshot,
    device_id: Option<kdevice::DeviceId>,
    neighbors: Vec<crate::ip::IpAddress>,
}

impl TestDevice {
    fn new(link: LinkSnapshot) -> Self {
        Self {
            link,
            device_id: None,
            neighbors: Vec::new(),
        }
    }

    fn with_device_id(mut self, device_id: kdevice::DeviceId) -> Self {
        self.device_id = Some(device_id);
        self
    }
}

impl NetDevice for TestDevice {
    fn name(&self) -> &str {
        &self.link.name
    }

    fn link_kind(&self) -> LinkKind {
        self.link.kind
    }

    fn mtu(&self) -> usize {
        self.link.mtu
    }

    fn is_link_up(&self) -> bool {
        self.link.flags & LINK_FLAG_UP != 0
    }

    fn link_snapshot(&self, ifindex: i32) -> LinkSnapshot {
        LinkSnapshot {
            ifindex,
            ..self.link.clone()
        }
    }

    fn link_send_snapshot(&self) -> LinkSendSnapshot {
        LinkSendSnapshot {
            is_up: self.is_link_up(),
            mtu: self.link.mtu,
            hardware_addr: self.link.hardware_addr,
        }
    }

    fn device_id(&self) -> Option<kdevice::DeviceId> {
        self.device_id
    }

    fn poll_rx(&mut self, _ifindex: i32, _timestamp: MonotonicInstant) -> Option<PacketBuf> {
        None
    }

    fn has_rx_work(&self) -> bool {
        false
    }

    fn send_ip_packet(
        &mut self,
        _ifindex: i32,
        _next_hop: crate::ip::IpAddress,
        _source_addr: crate::ip::IpAddress,
        _packet: PacketBuf,
        _timestamp: MonotonicInstant,
    ) -> bool {
        false
    }

    fn register_rx_waker(
        &self,
        _source_waker: &Waker,
        _context: &mut kpoll::PollContext<'_>,
    ) -> Result<(), kpoll::PollRegisterError> {
        Ok(())
    }

    fn set_name(&mut self, name: String) {
        self.link.name = name;
    }

    fn set_mtu(&mut self, mtu: usize) -> Result<(), LinuxError> {
        self.link.kind.validate_mtu(mtu)?;
        self.link.mtu = mtu;
        Ok(())
    }

    fn set_link_up(&mut self, is_up: bool) {
        if is_up {
            self.link.flags |= LINK_FLAG_UP | LINK_FLAG_RUNNING | LINK_FLAG_LOWER_UP;
            self.link.operstate = if self.link.kind == LinkKind::Ethernet {
                IF_OPER_UP
            } else {
                IF_OPER_UNKNOWN
            };
        } else {
            self.link.flags &= !(LINK_FLAG_UP | LINK_FLAG_RUNNING | LINK_FLAG_LOWER_UP);
            self.link.operstate = IF_OPER_DOWN;
        }
    }

    fn apply_neighbor_update(&mut self, update: NeighborUpdate) -> Result<(), LinuxError> {
        if !self.neighbors.contains(&update.dst) {
            self.neighbors.push(update.dst);
        }
        Ok(())
    }

    fn has_neighbor(&self, dst: crate::ip::IpAddress) -> bool {
        self.neighbors.contains(&dst)
    }
}

fn init_test_state() {
    let mut router = Router::new();
    router.add_device(Box::new(TestDevice::new(LinkSnapshot {
        ifindex: 1,
        name: String::from("lo"),
        flags: LINK_FLAG_UP | LINK_FLAG_RUNNING | LINK_FLAG_LOOPBACK | LINK_FLAG_LOWER_UP,
        mtu: 65_536,
        operstate: IF_OPER_UNKNOWN,
        kind: LinkKind::Loopback,
        hardware_addr: [0; 6],
        broadcast_addr: [0; 6],
    })));
    router.add_device(Box::new(
        TestDevice::new(LinkSnapshot {
            ifindex: 2,
            name: String::from("eth0"),
            flags: LINK_FLAG_UP
                | LINK_FLAG_RUNNING
                | LINK_FLAG_BROADCAST
                | LINK_FLAG_MULTICAST
                | LINK_FLAG_LOWER_UP,
            mtu: 1500,
            operstate: IF_OPER_UP,
            kind: LinkKind::Ethernet,
            hardware_addr: [0x02, 0x00, 0x00, 0x00, 0x00, 0x02],
            broadcast_addr: [0xff; 6],
        })
        .with_device_id(kdevice::DeviceId::new(2)),
    ));
    router
        .add_ipv4_addr(Ipv4AddrEntry {
            dev: 0,
            addr: crate::ip::Ipv4Cidr::new(crate::ip::Ipv4Address::new(127, 0, 0, 1), 8),
            scope: wire_route::SCOPE_HOST,
        })
        .unwrap();
    router.add_rule(Rule::new(
        crate::ip::Ipv4Cidr::new(crate::ip::Ipv4Address::UNSPECIFIED, 0).into(),
        Some(crate::ip::IpAddress::Ipv4(crate::ip::Ipv4Address::new(
            192, 168, 1, 1,
        ))),
        1,
        crate::ip::Ipv4Address::new(192, 168, 1, 2).into(),
    ));
    router
        .add_ipv4_addr(Ipv4AddrEntry {
            dev: 1,
            addr: crate::ip::Ipv4Cidr::new(crate::ip::Ipv4Address::new(192, 168, 1, 2), 24),
            scope: wire_route::SCOPE_UNIVERSE,
        })
        .unwrap();
    if SERVICE.is_inited() {
        SERVICE.replace_router_for_tests(router);
    } else {
        SERVICE.init_once(Service::new(router));
    }
}

fn nl_header(msg_type: u16, flags: u16, seq: u32) -> NlMsgHeader {
    NlMsgHeader {
        len: NLMSG_HDR_LEN as u32,
        msg_type,
        flags,
        seq,
        pid: 0,
    }
}

fn attr(kind: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    push_attr(&mut out, kind, payload);
    out
}

fn build_ipv4_addr_mutation(address: Ipv4Address, seq: u32, flags: u16) -> Vec<u8> {
    build_ipv4_addr_mutation_with_ifa_flags(address, seq, flags, 0)
}

fn build_ipv4_addr_mutation_with_ifa_flags(
    address: Ipv4Address,
    seq: u32,
    flags: u16,
    ifa_flags: u8,
) -> Vec<u8> {
    let mut payload = Vec::new();
    push_u8(&mut payload, wire_route::FAMILY_IPV4);
    push_u8(&mut payload, 24);
    push_u8(&mut payload, ifa_flags);
    push_u8(&mut payload, wire_route::SCOPE_UNIVERSE);
    push_u32_ne(&mut payload, 2);
    payload.extend(attr(wire_addr::attr::LOCAL, &address.octets()));
    build_nlmsg(RTM_NEWADDR, seq, flags, payload)
}

#[def_test]
fn test_rtnetlink_operation_classifies_supported_mutations() {
    for msg_type in [
        RTM_NEWLINK,
        RTM_NEWADDR,
        RTM_DELADDR,
        RTM_NEWROUTE,
        RTM_DELROUTE,
        RTM_NEWNEIGH,
    ] {
        assert!(rtnetlink_request_requires_privilege(msg_type));
    }
    for msg_type in [RTM_GETLINK, RTM_GETADDR, RTM_GETROUTE, u16::MAX] {
        assert!(!rtnetlink_request_requires_privilege(msg_type));
    }
}

#[def_test]
fn test_malformed_unsupported_protocol_request_does_not_queue_empty_response() {
    let socket = NetlinkSocket::new(i32::MAX);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 50, groups: 0 }))
        .unwrap();
    let mut request = build_nlmsg(u16::MAX, 61, NLM_F_REQUEST, Vec::new());
    let oversized_len = (request.len() as u32).checked_add(4).unwrap();
    request[..4].copy_from_slice(&oversized_len.to_ne_bytes());

    assert_eq!(
        socket.send(Cursor::new(request.as_slice()), SendOptions::default()),
        Ok(request.len())
    );
    assert!(socket.inner.rx_queue.lock().is_empty());
}

#[def_test(serial)]
fn test_router_owns_configured_routes() {
    init_test_state();
    assert!(SERVICE.route_snapshot().iter().any(|route| {
        route.is_configured()
            && route.filter.prefix_len() == 0
            && route.via
                == Some(crate::ip::IpAddress::Ipv4(crate::ip::Ipv4Address::new(
                    192, 168, 1, 1,
                )))
    }));
}

#[def_test(serial)]
fn test_parse_link_update_name_and_mtu() {
    let mut payload = Vec::new();
    push_u8(&mut payload, 17);
    push_u8(&mut payload, 0);
    push_u16_ne(&mut payload, ARPHRD_ETHER);
    push_i32_ne(&mut payload, 2);
    push_u32_ne(&mut payload, LINK_FLAG_UP);
    push_u32_ne(&mut payload, LINK_FLAG_UP);
    payload.extend(attr(wire_link::attr::IFNAME, b"eth1\0"));
    payload.extend(attr(wire_link::attr::MTU, &1400u32.to_ne_bytes()));

    let request = parse_link_update(&payload).unwrap();
    assert_eq!(request.index, 2);
    assert_eq!(request.name, Some(String::from("eth1")));
    assert_eq!(request.mtu, Some(1400));
}

#[def_test]
fn test_parse_link_update_rejects_unsupported_attributes() {
    let mut payload = Vec::new();
    push_u8(&mut payload, 17);
    push_u8(&mut payload, 0);
    push_u16_ne(&mut payload, ARPHRD_ETHER);
    push_i32_ne(&mut payload, 2);
    push_u32_ne(&mut payload, 0);
    push_u32_ne(&mut payload, 0);
    payload.extend(attr(wire_link::attr::ADDRESS, &[1, 2, 3, 4, 5, 6]));

    assert!(matches!(
        parse_link_update(&payload),
        Err(LinuxError::EOPNOTSUPP)
    ));
}

#[def_test(serial)]
fn test_apply_link_update_rename_and_down() {
    init_test_state();
    let request = LinkUpdateRequest {
        index: 2,
        flags: 0,
        change: LINK_FLAG_UP,
        name: Some(String::from("ens3")),
        mtu: Some(1400),
    };
    apply_link_update(request).unwrap();

    let link = SERVICE
        .link_snapshot_for_ifindex(2)
        .expect("eth0 link snapshot");
    assert_eq!(link.name, "ens3");
    assert_eq!(link.mtu, 1400);
    assert_eq!(link.flags & LINK_FLAG_UP, 0);
}

#[def_test(serial)]
fn test_device_mtu_is_route_mtu_source() {
    init_test_state();
    let destination = crate::ip::IpAddress::Ipv4(crate::ip::Ipv4Address::new(8, 8, 8, 8));
    assert_eq!(SERVICE.ipv4_route_mtu(&destination), Some(1500));

    apply_link_update(LinkUpdateRequest {
        index: 2,
        flags: 0,
        change: 0,
        name: None,
        mtu: Some(1400),
    })
    .unwrap();

    assert_eq!(SERVICE.ipv4_route_mtu(&destination), Some(1400));
    assert!(
        SERVICE
            .route_snapshot()
            .iter()
            .any(|route| route.is_configured())
    );
}

#[def_test(serial)]
fn test_unchanged_mtu_does_not_rebuild_interface() {
    init_test_state();
    let rebuild_count = SERVICE.rebuild_count_for_tests();

    apply_link_update(LinkUpdateRequest {
        index: 2,
        flags: 0,
        change: 0,
        name: None,
        mtu: Some(1500),
    })
    .unwrap();

    assert_eq!(SERVICE.rebuild_count_for_tests(), rebuild_count);
}

#[def_test(serial)]
fn test_down_device_keeps_route_mtu_for_output_checks() {
    init_test_state();
    let destination = crate::ip::IpAddress::Ipv4(crate::ip::Ipv4Address::new(8, 8, 8, 8));

    apply_link_update(LinkUpdateRequest {
        index: 2,
        flags: 0,
        change: LINK_FLAG_UP,
        name: None,
        mtu: None,
    })
    .unwrap();

    assert_eq!(SERVICE.ipv4_route_mtu(&destination), Some(1500));
    assert_eq!(
        SERVICE.get_source_address(&destination),
        Err(LinuxError::ENETUNREACH.into())
    );
}

#[def_test(serial)]
fn test_non_effective_mtu_change_does_not_rebuild_interface() {
    init_test_state();
    let rebuild_count = SERVICE.rebuild_count_for_tests();

    apply_link_update(LinkUpdateRequest {
        index: 1,
        flags: 0,
        change: 0,
        name: None,
        mtu: Some(60_000),
    })
    .unwrap();

    assert_eq!(SERVICE.rebuild_count_for_tests(), rebuild_count);
    assert_eq!(
        SERVICE.ipv4_route_mtu(&crate::ip::IpAddress::Ipv4(crate::ip::Ipv4Address::new(
            127, 0, 0, 1
        ))),
        Some(60_000)
    );
}

#[def_test(serial)]
fn test_link_change_mask_ignores_volatile_flags() {
    init_test_state();

    apply_link_update(LinkUpdateRequest {
        index: 2,
        flags: 0,
        change: LINK_FLAG_RUNNING,
        name: None,
        mtu: None,
    })
    .unwrap();
    let current_flags = SERVICE
        .link_snapshot_for_ifindex(2)
        .expect("eth0 link snapshot")
        .flags;
    assert_ne!(current_flags & LINK_FLAG_RUNNING, 0);

    apply_link_update(LinkUpdateRequest {
        index: 2,
        flags: current_flags & !LINK_FLAG_UP,
        change: u32::MAX,
        name: None,
        mtu: None,
    })
    .unwrap();
    let down_flags = SERVICE
        .link_snapshot_for_ifindex(2)
        .expect("eth0 link snapshot")
        .flags;
    assert_eq!(down_flags & LINK_FLAG_UP, 0);
    assert_ne!(down_flags & LINK_FLAG_MULTICAST, 0);

    assert_eq!(
        apply_link_update(LinkUpdateRequest {
            index: 2,
            flags: down_flags & !LINK_FLAG_MULTICAST,
            change: LINK_FLAG_MULTICAST,
            name: None,
            mtu: None,
        }),
        Err(LinuxError::EOPNOTSUPP)
    );
}

#[def_test(serial)]
fn test_device_removal_refreshes_views() {
    init_test_state();
    let service = &SERVICE;
    let rebuild_count = service.rebuild_count_for_tests();
    add_neigh(
        NeighRequest {
            family: wire_route::FAMILY_IPV4,
            ifindex: 2,
            state: wire_neigh::STATE_PERMANENT,
            flags: 0,
            dst: IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 10)),
            lladdr: Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]),
        },
        NLM_F_CREATE,
    )
    .unwrap();

    assert!(crate::unregister_netdev(kdevice::DeviceId::new(2)));
    assert_eq!(service.rebuild_count_for_tests(), rebuild_count + 1);
    assert_eq!(service.link_snapshots().len(), 1);
    assert_eq!(
        service.iface.lock().ip_addrs(),
        &[IpCidr::Ipv4(smoltcp::wire::Ipv4Cidr::new(
            Ipv4Address::new(127, 0, 0, 1),
            8,
        ))]
    );
    assert!(SERVICE.route_snapshot().iter().all(|route| route.dev != 1));
    assert!(!SERVICE.has_neighbor(
        1,
        crate::ip::IpAddress::Ipv4(crate::ip::Ipv4Address::new(192, 168, 1, 10)),
    ));
}

#[def_test(serial)]
fn test_reject_invalid_link_name_and_mtu_without_partial_update() {
    init_test_state();

    for name in [
        "",
        ".",
        "..",
        "eth/1",
        "eth:1",
        "eth 1",
        "eth\x0b1",
        "1234567890123456",
    ] {
        assert_eq!(
            apply_link_update(LinkUpdateRequest {
                index: 2,
                flags: 0,
                change: 0,
                name: Some(String::from(name)),
                mtu: None,
            }),
            Err(LinuxError::EINVAL)
        );
    }
    assert_eq!(
        apply_link_update(LinkUpdateRequest {
            index: 2,
            flags: 0,
            change: 0,
            name: Some(String::from("ens3")),
            mtu: Some(1501),
        }),
        Err(LinuxError::EINVAL)
    );

    let link = SERVICE
        .link_snapshot_for_ifindex(2)
        .expect("eth0 remains present");
    assert_eq!(link.name, "eth0");
    assert_eq!(link.mtu, 1500);
    assert_ne!(link.flags & LINK_FLAG_UP, 0);

    assert_eq!(
        apply_link_update(LinkUpdateRequest {
            index: 2,
            flags: 0,
            change: 0,
            name: Some(String::from("lo")),
            mtu: None,
        }),
        Err(LinuxError::EEXIST)
    );
}

#[def_test(serial)]
fn test_link_dump_reads_device_snapshots() {
    init_test_state();
    let packets =
        handle_rtnetlink_request(&build_nlmsg(RTM_GETLINK, 17, NLM_F_REQUEST, Vec::new()));

    assert_eq!(packets.len(), 3);
    let links: Vec<_> = packets
        .iter()
        .filter_map(|packet| {
            let header = NlMsgHeader::read(&packet.data)?;
            (header.msg_type == RTM_NEWLINK).then(|| {
                let payload = &packet.data[NLMSG_HDR_LEN..];
                let info = IfInfoMsg::read(payload).expect("valid RTM_NEWLINK payload");
                let change = info.change;
                assert_eq!(info.family, 0);
                assert_eq!(change, 0);
                assert!(
                    parse_attrs(&payload[IfInfoMsg::SIZE..])
                        .expect("valid link attributes")
                        .iter()
                        .all(|attr| attr.kind != wire_link::attr::LINK)
                );
                info.index
            })
        })
        .collect();
    assert_eq!(links, vec![1, 2]);
}

#[def_test(serial)]
fn test_addr_dump_reads_router_addresses() {
    init_test_state();
    let packets =
        handle_rtnetlink_request(&build_nlmsg(RTM_GETADDR, 18, NLM_F_REQUEST, Vec::new()));
    let mut addresses = Vec::new();
    for packet in packets {
        let header = NlMsgHeader::read(&packet.data).unwrap();
        if header.msg_type != RTM_NEWADDR {
            continue;
        }
        let payload = &packet.data[NLMSG_HDR_LEN..];
        let info = IfAddrMsg::read(payload).unwrap();
        let address = parse_attrs(&payload[IfAddrMsg::SIZE..])
            .unwrap()
            .into_iter()
            .find(|attr| attr.kind == wire_addr::attr::ADDRESS)
            .map(|attr| attr.payload.to_vec())
            .unwrap();
        addresses.push((info.index, info.prefix_len, address));
    }

    assert_eq!(addresses.len(), 2);
    assert!(addresses.iter().any(|(index, prefix_len, address)| {
        *index == 1 && *prefix_len == 8 && address == &vec![127, 0, 0, 1]
    }));
    assert!(addresses.iter().any(|(index, prefix_len, address)| {
        *index == 2 && *prefix_len == 24 && address == &vec![192, 168, 1, 2]
    }));
}

#[def_test(serial)]
fn test_add_replace_and_delete_address() {
    init_test_state();
    let req = AddrRequest {
        index: 2,
        family: wire_route::FAMILY_IPV4,
        prefix_len: 24,
        scope: wire_route::SCOPE_UNIVERSE,
        address: IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 99)),
    };

    add_addr(req, NLM_F_CREATE | NLM_F_EXCL).unwrap();
    assert!(
        SERVICE
            .ipv4_addr_entries()
            .iter()
            .any(
                |entry| entry.addr.address() == crate::ip::Ipv4Address::new(192, 168, 1, 99)
                    && entry.dev == 1
            )
    );

    let replaced = AddrRequest {
        scope: wire_route::SCOPE_HOST,
        ..req
    };
    add_addr(replaced, NLM_F_REPLACE).unwrap();
    assert!(
        SERVICE
            .ipv4_addr_entries()
            .iter()
            .any(
                |entry| entry.addr.address() == crate::ip::Ipv4Address::new(192, 168, 1, 99)
                    && entry.scope == wire_route::SCOPE_UNIVERSE
            )
    );

    del_addr(replaced).unwrap();
    assert!(
        !SERVICE
            .ipv4_addr_entries()
            .iter()
            .any(
                |entry| entry.addr.address() == crate::ip::Ipv4Address::new(192, 168, 1, 99)
                    && entry.dev == 1
            )
    );
}

#[def_test(serial)]
fn test_deleting_last_address_owner_removes_configured_prefsrc_route() {
    init_test_state();
    let address = Ipv4Address::new(192, 168, 1, 2);
    del_addr(AddrRequest {
        index: 2,
        family: wire_route::FAMILY_IPV4,
        prefix_len: 24,
        scope: wire_route::SCOPE_UNIVERSE,
        address: IpAddress::Ipv4(address),
    })
    .unwrap();

    assert!(SERVICE.route_snapshot().iter().all(|route| {
        route.preferred_source() != Some(crate::ip::IpAddress::Ipv4(address.into()))
    }));

    add_neigh(
        NeighRequest {
            family: wire_route::FAMILY_IPV4,
            ifindex: 2,
            state: wire_neigh::STATE_PERMANENT,
            flags: 0,
            dst: IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 10)),
            lladdr: Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]),
        },
        NLM_F_CREATE,
    )
    .unwrap();

    assert_eq!(
        SERVICE.get_source_address(&crate::ip::IpAddress::Ipv4(crate::ip::Ipv4Address::new(
            8, 8, 8, 8
        ),)),
        Err(LinuxError::ENETUNREACH.into())
    );
}

#[def_test(serial)]
fn test_add_and_delete_route() {
    init_test_state();
    let req = RouteRequest {
        family: wire_route::FAMILY_IPV4,
        dst_len: 24,
        src_len: 0,
        table: RT_TABLE_MAIN,
        protocol: wire_route::PROTOCOL_BOOT,
        scope: wire_route::SCOPE_UNIVERSE,
        route_type: RTN_UNICAST,
        oif: 2,
        dst: Some(IpAddress::Ipv4(Ipv4Address::new(172, 16, 0, 0))),
        src: None,
        gateway: Some(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 1))),
        prefsrc: Some(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 2))),
    };

    add_route(req, NLM_F_CREATE | NLM_F_EXCL).unwrap();
    assert!(
        SERVICE
            .route_snapshot()
            .iter()
            .any(|route| route.filter.prefix_len() == req.dst_len && route.via.is_some())
    );

    del_route(req).unwrap();
    assert!(
        !SERVICE
            .route_snapshot()
            .iter()
            .any(|route| route.filter.prefix_len() == req.dst_len && route.via.is_some())
    );
}

#[def_test(serial)]
fn test_newroute_ignores_unknown_attributes() {
    init_test_state();
    let dst = Ipv4Address::new(172, 16, 1, 0);
    let gateway = Ipv4Address::new(192, 168, 1, 1);
    let mut payload = Vec::new();
    push_u8(&mut payload, wire_route::FAMILY_IPV4);
    push_u8(&mut payload, 24);
    push_u8(&mut payload, 0);
    push_u8(&mut payload, 0);
    push_u8(&mut payload, RT_TABLE_MAIN);
    push_u8(&mut payload, wire_route::PROTOCOL_BOOT);
    push_u8(&mut payload, wire_route::SCOPE_UNIVERSE);
    push_u8(&mut payload, RTN_UNICAST);
    push_u32_ne(&mut payload, 0);
    payload.extend(attr(wire_route::attr::DST, &dst.octets()));
    payload.extend(attr(wire_route::attr::OIF, &2u32.to_ne_bytes()));
    payload.extend(attr(wire_route::attr::GATEWAY, &gateway.octets()));
    // RTA_PRIORITY and a type beyond current RTA_MAX.
    payload.extend(attr(6, &100u32.to_ne_bytes()));
    payload.extend(attr(99, &0u32.to_ne_bytes()));

    let packets = handle_rtnetlink_request(&build_nlmsg(
        RTM_NEWROUTE,
        21,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        payload,
    ));

    assert_eq!(packets.len(), 1);
    assert_eq!(read_u16_ne(&packets[0].data, 4), Some(NLMSG_ERROR));
    assert_eq!(read_i32_ne(&packets[0].data, NLMSG_HDR_LEN), Some(0));
    assert!(SERVICE.route_snapshot().iter().any(|route| {
        route.filter.prefix_len() == 24
            && route.via
                == Some(crate::ip::IpAddress::Ipv4(crate::ip::Ipv4Address::new(
                    192, 168, 1, 1,
                )))
    }));
}

#[def_test(serial)]
fn test_add_and_replace_neighbour() {
    init_test_state();
    let req = NeighRequest {
        family: wire_route::FAMILY_IPV4,
        ifindex: 2,
        state: wire_neigh::STATE_PERMANENT,
        flags: 0,
        dst: IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 10)),
        lladdr: Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]),
    };

    add_neigh(req, NLM_F_CREATE | NLM_F_REPLACE).unwrap();
    assert!(SERVICE.has_neighbor(
        1,
        crate::ip::IpAddress::Ipv4(match req.dst {
            IpAddress::Ipv4(address) => address.into(),
            IpAddress::Ipv6(_) => panic!("IPv4 test neighbor"),
        }),
    ));

    assert_eq!(
        add_neigh(req, NLM_F_CREATE | NLM_F_EXCL),
        Err(LinuxError::EEXIST)
    );

    let replaced = NeighRequest {
        lladdr: Some([1, 2, 3, 4, 5, 6]),
        ..req
    };
    add_neigh(replaced, NLM_F_REPLACE).unwrap();
    assert!(SERVICE.has_neighbor(
        1,
        crate::ip::IpAddress::Ipv4(match replaced.dst {
            IpAddress::Ipv4(address) => address.into(),
            IpAddress::Ipv6(_) => panic!("IPv4 test neighbor"),
        }),
    ));

    let missing = NeighRequest {
        dst: IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 11)),
        ..req
    };
    assert_eq!(add_neigh(missing, NLM_F_REPLACE), Err(LinuxError::ENOENT));
}

#[def_test(serial)]
fn test_dump_routes_returns_done_message() {
    init_test_state();
    let header = nl_header(RTM_GETROUTE, NLM_F_REQUEST, 7);
    let packets = handle_rtnetlink_request(&build_nlmsg(
        RTM_GETROUTE,
        7,
        NLM_F_REQUEST,
        vec![wire_route::FAMILY_IPV4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    ));

    assert!(packets.len() >= 2);
    let last = packets.last().unwrap();
    let seq = header.seq;
    assert_eq!(read_u16_ne(&last.data, 4), Some(NLMSG_DONE));
    assert_eq!(seq, 7);
}

#[def_test(serial)]
fn test_ack_response_for_newaddr() {
    init_test_state();
    let mut payload = Vec::new();
    push_u8(&mut payload, wire_route::FAMILY_IPV4);
    push_u8(&mut payload, 24);
    push_u8(&mut payload, 0);
    push_u8(&mut payload, wire_route::SCOPE_UNIVERSE);
    push_u32_ne(&mut payload, 2);
    payload.extend(attr(wire_addr::attr::LOCAL, &[192, 168, 1, 77]));

    let request = build_nlmsg(
        RTM_NEWADDR,
        11,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE,
        payload,
    );
    let packets = handle_rtnetlink_request(&request);

    assert_eq!(packets.len(), 1);
    assert_eq!(read_u16_ne(&packets[0].data, 4), Some(NLMSG_ERROR));
    assert_eq!(read_i32_ne(&packets[0].data, NLMSG_HDR_LEN), Some(0));
}

#[def_test(serial)]
fn test_newaddr_accepts_legacy_ipv4_address_flags() {
    init_test_state();
    let address = Ipv4Address::new(192, 168, 1, 76);
    let request = build_ipv4_addr_mutation_with_ifa_flags(
        address,
        12,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        wire_addr::FLAG_PERMANENT,
    );

    let packets = handle_rtnetlink_request(&request);

    assert_eq!(packets.len(), 1);
    assert_eq!(read_i32_ne(&packets[0].data, NLMSG_HDR_LEN), Some(0));
    assert!(
        SERVICE
            .ipv4_addr_entries()
            .iter()
            .any(|entry| entry.addr.address() == address.into())
    );
}

#[def_test(serial)]
fn test_publish_kobject_uevent_to_group_subscriber() {
    let socket = NetlinkSocket::new(NETLINK_KOBJECT_UEVENT);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 0, groups: 1 }))
        .unwrap();

    let payload = b"add@/devices/test\0ACTION=add\0SUBSYSTEM=block\0DEVPATH=/devices/test\0";
    publish_kobject_uevent(1, payload);

    let packet = socket.inner.rx_queue.lock().pop_front().unwrap();
    assert_eq!(packet.from.groups, 1);
    assert!(packet.data.starts_with(payload));
    assert!(packet.data[payload.len()..].starts_with(b"SEQNUM="));
    assert_eq!(packet.data.last(), Some(&0));
}

#[def_test(serial)]
fn test_publish_kobject_uevent_drops_when_rx_queue_limit_exceeded() {
    let socket = NetlinkSocket::new(NETLINK_KOBJECT_UEVENT);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 0, groups: 1 }))
        .unwrap();

    let next_seqnum = UEVENT_SEQNUM.load(Ordering::Relaxed) + 1;
    let seqnum_suffix_len = format!("SEQNUM={next_seqnum}\0").len();
    let payload = vec![0u8; NETLINK_RX_QUEUE_LIMIT - seqnum_suffix_len];
    publish_kobject_uevent(1, &payload);
    publish_kobject_uevent(1, b"overflow");

    let mut queue = socket.inner.rx_queue.lock();
    let packet = queue.pop_front().unwrap();
    assert!(packet.data.starts_with(&payload));
    assert!(queue.pop_front().is_none());
}

#[def_test(serial)]
fn test_publish_kobject_uevent_appends_monotonic_seqnum() {
    let socket = NetlinkSocket::new(NETLINK_KOBJECT_UEVENT);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 0, groups: 1 }))
        .unwrap();

    publish_kobject_uevent(1, b"first\0");
    publish_kobject_uevent(1, b"second\0");

    let mut queue = socket.inner.rx_queue.lock();
    let first = queue.pop_front().unwrap();
    let second = queue.pop_front().unwrap();

    let first_seq = core::str::from_utf8(&first.data)
        .unwrap()
        .split('\0')
        .find_map(|entry| entry.strip_prefix("SEQNUM="))
        .unwrap()
        .parse::<u64>()
        .unwrap();
    let second_seq = core::str::from_utf8(&second.data)
        .unwrap()
        .split('\0')
        .find_map(|entry| entry.strip_prefix("SEQNUM="))
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert!(second_seq > first_seq);
}

#[def_test(serial)]
fn test_recv_preserves_packet_when_dst_buffer_too_small() {
    let socket = NetlinkSocket::new(NETLINK_ROUTE);
    socket.inner.rx_queue.lock().push_back(NetlinkPacket {
        from: NetlinkAddr { pid: 1, groups: 0 },
        data: vec![1, 2, 3, 4],
    });

    let mut buf = [0u8; 2];
    let result = socket.recv(Cursor::new(buf.as_mut_slice()), RecvOptions::default());

    assert_eq!(result, Err(LinuxError::EMSGSIZE.into()));

    let mut queue = socket.inner.rx_queue.lock();
    let packet = queue.pop_front().unwrap();
    assert_eq!(packet.data, vec![1, 2, 3, 4]);
    assert!(queue.pop_front().is_none());
}

#[def_test(serial)]
fn test_route_mutation_checks_each_send_credential() {
    init_test_state();
    let socket = NetlinkSocket::new(NETLINK_ROUTE);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 42, groups: 0 }))
        .unwrap();
    let mutation = build_nlmsg(RTM_NEWLINK, 41, NLM_F_REQUEST | NLM_F_ACK, Vec::new());
    let unprivileged = Cred::new(1000, 1000);
    assert_eq!(
        socket.send_with_cred(
            Cursor::new(mutation.as_slice()),
            SendOptions::default(),
            &unprivileged,
        ),
        Ok(mutation.len())
    );
    let permission_error = socket.inner.rx_queue.lock().pop_front().unwrap();
    assert_eq!(
        read_i32_ne(&permission_error.data, NLMSG_HDR_LEN),
        Some(-(LinuxError::EPERM.into_raw()))
    );

    let query = build_nlmsg(RTM_GETLINK, 42, NLM_F_REQUEST, Vec::new());
    assert_eq!(
        socket.send_with_cred(
            Cursor::new(query.as_slice()),
            SendOptions::default(),
            &unprivileged,
        ),
        Ok(query.len())
    );
    while socket.inner.rx_queue.lock().pop_front().is_some() {}
    assert_eq!(
        socket.send_with_cred(
            Cursor::new(mutation.as_slice()),
            SendOptions::default(),
            &Cred::root(),
        ),
        Ok(mutation.len())
    );
    let invalid_request = socket.inner.rx_queue.lock().pop_front().unwrap();
    assert_eq!(
        read_i32_ne(&invalid_request.data, NLMSG_HDR_LEN),
        Some(-(LinuxError::EINVAL.into_raw()))
    );
}

#[def_test(serial)]
fn test_route_socket_processes_same_class_mutation_batch() {
    init_test_state();
    let socket = NetlinkSocket::new(NETLINK_ROUTE);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 43, groups: 0 }))
        .unwrap();
    let flags = NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL;
    let first_addr = Ipv4Address::new(192, 168, 1, 77);
    let second_addr = Ipv4Address::new(192, 168, 1, 78);
    let first = build_ipv4_addr_mutation(first_addr, 51, flags);
    let second = build_ipv4_addr_mutation(second_addr, 52, flags);
    let mut datagram = first;
    datagram.extend_from_slice(&second);

    assert_eq!(
        socket.send_with_cred(
            Cursor::new(datagram.as_slice()),
            SendOptions::default(),
            &Cred::root(),
        ),
        Ok(datagram.len())
    );

    let mut queue = socket.inner.rx_queue.lock();
    let first_ack = queue.pop_front().unwrap();
    let second_ack = queue.pop_front().unwrap();
    assert_eq!(read_i32_ne(&first_ack.data, NLMSG_HDR_LEN), Some(0));
    assert_eq!(read_i32_ne(&second_ack.data, NLMSG_HDR_LEN), Some(0));
    assert!(queue.pop_front().is_none());
    drop(queue);

    assert!(
        SERVICE
            .ipv4_addr_entries()
            .iter()
            .any(|entry| { entry.addr.address() == first_addr.into() && entry.dev == 1 })
    );
    assert!(
        SERVICE
            .ipv4_addr_entries()
            .iter()
            .any(|entry| { entry.addr.address() == second_addr.into() && entry.dev == 1 })
    );
}

#[def_test(serial)]
fn test_route_socket_processes_query_batch() {
    init_test_state();
    let socket = NetlinkSocket::new(NETLINK_ROUTE);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 47, groups: 0 }))
        .unwrap();
    let mut datagram = build_nlmsg(RTM_GETLINK, 57, NLM_F_REQUEST, Vec::new());
    datagram.extend_from_slice(&build_nlmsg(RTM_GETADDR, 58, NLM_F_REQUEST, Vec::new()));

    assert_eq!(
        socket.send_with_cred(
            Cursor::new(datagram.as_slice()),
            SendOptions::default(),
            &Cred::root(),
        ),
        Ok(datagram.len())
    );
    let mut done_sequences = Vec::new();
    let mut queue = socket.inner.rx_queue.lock();
    while let Some(packet) = queue.pop_front() {
        let header = NlMsgHeader::read(&packet.data).unwrap();
        if header.msg_type == NLMSG_DONE {
            done_sequences.push(header.seq);
        }
    }
    assert_eq!(done_sequences, vec![57, 58]);
}

#[def_test(serial)]
fn test_route_socket_rejects_mixed_query_and_mutation_batch() {
    init_test_state();
    let socket = NetlinkSocket::new(NETLINK_ROUTE);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 48, groups: 0 }))
        .unwrap();
    let address = Ipv4Address::new(192, 168, 1, 83);
    let mut datagram = build_nlmsg(RTM_GETLINK, 59, NLM_F_REQUEST, Vec::new());
    datagram.extend_from_slice(&build_ipv4_addr_mutation(
        address,
        60,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
    ));

    assert_eq!(
        socket.send_with_cred(
            Cursor::new(datagram.as_slice()),
            SendOptions::default(),
            &Cred::root(),
        ),
        Err(LinuxError::EOPNOTSUPP.into())
    );
    assert!(socket.inner.rx_queue.lock().is_empty());
    assert!(
        !SERVICE
            .ipv4_addr_entries()
            .iter()
            .any(|entry| { entry.addr.address() == address.into() && entry.dev == 1 })
    );
}

#[def_test(serial)]
fn test_malformed_nlmsg_len_does_not_mutate_network_state() {
    init_test_state();
    let socket = NetlinkSocket::new(NETLINK_ROUTE);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 44, groups: 0 }))
        .unwrap();
    let address = Ipv4Address::new(192, 168, 1, 79);
    let mut request = build_ipv4_addr_mutation(
        address,
        53,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
    );
    let oversized_len = (request.len() as u32).checked_add(4).unwrap();
    request[..4].copy_from_slice(&oversized_len.to_ne_bytes());

    assert_eq!(
        socket.send_with_cred(
            Cursor::new(request.as_slice()),
            SendOptions::default(),
            &Cred::root(),
        ),
        Err(LinuxError::EINVAL.into())
    );
    assert!(socket.inner.rx_queue.lock().is_empty());
    assert!(
        !SERVICE
            .ipv4_addr_entries()
            .iter()
            .any(|entry| { entry.addr.address() == address.into() && entry.dev == 1 })
    );
}

#[def_test(serial)]
fn test_route_socket_rejects_non_kernel_destination() {
    let socket = NetlinkSocket::new(NETLINK_ROUTE);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 45, groups: 0 }))
        .unwrap();
    let query = build_nlmsg(RTM_GETLINK, 54, NLM_F_REQUEST, Vec::new());

    for destination in [
        NetlinkAddr {
            pid: 1234,
            groups: 0,
        },
        NetlinkAddr { pid: 0, groups: 1 },
    ] {
        assert_eq!(
            socket.send_with_cred(
                Cursor::new(query.as_slice()),
                SendOptions {
                    to: Some(SocketAddrEx::Netlink(destination)),
                    ..SendOptions::default()
                },
                &Cred::root(),
            ),
            Err(LinuxError::EOPNOTSUPP.into())
        );
    }
    assert!(socket.inner.rx_queue.lock().is_empty());
}

#[def_test]
fn test_route_socket_rejects_unsupported_multicast_subscription() {
    let socket = NetlinkSocket::new(NETLINK_ROUTE);
    assert_eq!(
        socket.bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 49, groups: 1 })),
        Err(LinuxError::EOPNOTSUPP.into())
    );
    assert!(matches!(
        socket.local_addr(),
        Err(kerrno::KError::NotConnected)
    ));
}

#[def_test(serial)]
fn test_mutation_queue_reservation_failure_preserves_network_state() {
    init_test_state();
    let socket = NetlinkSocket::new(NETLINK_ROUTE);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 46, groups: 0 }))
        .unwrap();
    assert!(socket.inner.rx_queue.lock().push_back(NetlinkPacket {
        from: NetlinkAddr { pid: 0, groups: 0 },
        data: vec![0; NETLINK_RX_QUEUE_LIMIT],
    }));
    let address = Ipv4Address::new(192, 168, 1, 80);
    let request = build_ipv4_addr_mutation(
        address,
        55,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
    );

    assert_eq!(
        socket.send_with_cred(
            Cursor::new(request.as_slice()),
            SendOptions::default(),
            &Cred::root(),
        ),
        Err(LinuxError::ENOBUFS.into())
    );
    assert!(
        !SERVICE
            .ipv4_addr_entries()
            .iter()
            .any(|entry| { entry.addr.address() == address.into() && entry.dev == 1 })
    );
}
