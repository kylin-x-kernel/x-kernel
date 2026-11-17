// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use alloc::{format, string::String, vec, vec::Vec};
use core::{
    sync::atomic::Ordering,
    task::{Context, Waker},
};

use kcred::{CAP_NET_ADMIN, CAP_SYS_ADMIN, CapSet, CapSets, Cred};
use kerrno::LinuxError;
use kio::Cursor;
use kpoll::{IoEvents, PollRegistrations, Pollable};
use ktime_types::MonotonicInstant;
use smoltcp::wire::{IpAddress, IpCidr, Ipv4Address};
use unittest::def_test;

use super::{
    rtnetlink::*,
    socket::publish_kobject_uevent,
    wire::{
        IfAddrMsg, IfInfoMsg, NdMsg, NlMsgHeader, RtMsg, addr as wire_addr, build_nlmsg,
        link as wire_link, neigh as wire_neigh, parse_attrs, push_attr, push_i32_ne, push_u8,
        push_u16_ne, push_u32_ne, read_i32_ne, read_u16_ne, read_u32_ne, route as wire_route,
    },
    *,
};
use crate::{
    ConnectOptions, KernelAncillaryData, RecvFlags, RecvOptions, SERVICE, SendOptions,
    SocketAddrEx,
    buf::PacketBuf,
    device::{
        IF_OPER_DOWN, IF_OPER_UNKNOWN, IF_OPER_UP, LINK_FLAG_BROADCAST, LINK_FLAG_LOOPBACK,
        LINK_FLAG_LOWER_UP, LINK_FLAG_MULTICAST, LINK_FLAG_RUNNING, LINK_FLAG_UP, LinkKind,
        LinkSendSnapshot, LinkSnapshot, NeighborSnapshot, NeighborState, NeighborUpdate, NetDevice,
        NetDeviceId, RxPoll,
    },
    netlink::NetlinkSocketType,
    network::{event::NetworkEvent, interface_address::Ipv4AddrEntry, router::Rule},
    router::Router,
    service::Service,
    socket::{
        SocketOps,
        options::{Configurable, GetSocketOption, SetSocketOption},
    },
};

fn valid_ipv4_cidr(address: crate::ip::Ipv4Address, prefix_len: u8) -> crate::ip::Ipv4Cidr {
    crate::ip::Ipv4Cidr::new(address, prefix_len).unwrap()
}

fn credential_with_capabilities(uid: u32, effective: CapSet) -> Cred {
    let mut credentials = Cred::root();
    if uid != 0 {
        // The uid transition itself needs CAP_SETUID and CAP_SETPCAP in the
        // effective set, so perform it while the new root credentials still
        // hold their full capability sets. KEEP_CAPS preserves the permitted
        // set across the transition; the final transition below then narrows
        // it to the capability the test wants to exercise.
        credentials.keep_caps_enable().unwrap();
        credentials
            .set_resuid(Some(uid), Some(uid), Some(uid))
            .unwrap();
    }
    credentials
        .set_capabilities(CapSets::new(effective, effective, CapSet::EMPTY))
        .unwrap();
    credentials
}

fn capability(capability: u32) -> CapSet {
    CapSet::from_bits(1 << capability)
}

fn assert_socket_unbound(socket: &NetlinkSocket) {
    let SocketAddrEx::Netlink(addr) = socket.local_addr().unwrap() else {
        panic!("netlink local address");
    };
    assert_eq!(addr, NetlinkAddr::default());
}

struct TestDevice {
    link: LinkSnapshot,
    device_id: Option<kdevice::DeviceId>,
    publication_id: Option<kdevice::PublicationId>,
    neighbors: Vec<(crate::ip::IpAddress, NeighborState)>,
}

impl TestDevice {
    fn new(link: LinkSnapshot) -> Self {
        Self {
            link,
            device_id: None,
            publication_id: None,
            neighbors: Vec::new(),
        }
    }

    fn with_device_id(mut self, device_id: kdevice::DeviceId) -> Self {
        self.device_id = Some(device_id);
        self.publication_id =
            Some(kdevice::PublicationId::allocate().expect("test publication id"));
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

    fn publication_id(&self) -> Option<kdevice::PublicationId> {
        self.publication_id
    }

    fn poll_rx(&mut self, _ifindex: i32, _timestamp: MonotonicInstant) -> kerrno::KResult<RxPoll> {
        Ok(RxPoll::Empty)
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

    fn apply_neighbor_update(
        &mut self,
        update: NeighborUpdate,
        can_override: bool,
    ) -> Result<(), LinuxError> {
        let old = self.neighbor_state(update.dst);
        if !can_override
            && let (Some(old), Some(new)) = (
                old.and_then(NeighborState::hardware_addr),
                update.state.hardware_addr(),
            )
            && old != new
        {
            return Ok(());
        }
        match self
            .neighbors
            .iter_mut()
            .find(|(dst, _)| *dst == update.dst)
        {
            Some((_, state)) => *state = update.state,
            None => self.neighbors.push((update.dst, update.state)),
        }
        Ok(())
    }

    fn has_neighbor(&self, dst: crate::ip::IpAddress) -> bool {
        self.neighbors.iter().any(|(neighbor, _)| *neighbor == dst)
    }

    fn neighbor_state(&self, dst: crate::ip::IpAddress) -> Option<NeighborState> {
        self.neighbors
            .iter()
            .find(|(neighbor, _)| *neighbor == dst)
            .map(|(_, state)| *state)
    }

    fn neighbor_snapshots(&self, dev: NetDeviceId) -> Vec<crate::device::NeighborSnapshot> {
        self.neighbors
            .iter()
            .map(|(dst, state)| crate::device::NeighborSnapshot {
                dev,
                dst: *dst,
                state: *state,
            })
            .collect()
    }

    fn remove_neighbor(&mut self, dst: crate::ip::IpAddress) -> bool {
        let Some(index) = self
            .neighbors
            .iter()
            .position(|(neighbor, _)| *neighbor == dst)
        else {
            return false;
        };
        self.neighbors.remove(index);
        true
    }
}

fn init_test_state() {
    let mut router = Router::new();
    router
        .add_device(kalloc_types::KBox::new(TestDevice::new(LinkSnapshot {
            ifindex: 1,
            name: String::from("lo"),
            flags: LINK_FLAG_UP | LINK_FLAG_RUNNING | LINK_FLAG_LOOPBACK | LINK_FLAG_LOWER_UP,
            mtu: 65_536,
            operstate: IF_OPER_UNKNOWN,
            kind: LinkKind::Loopback,
            hardware_addr: [0; 6],
            broadcast_addr: [0; 6],
        })).unwrap())
        .unwrap();
    router
        .add_device(kalloc_types::KBox::new(
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
        ).unwrap())
        .unwrap();
    router
        .add_ipv4_addr(
            NetDeviceId::LOOPBACK,
            Ipv4AddrEntry {
                addr: valid_ipv4_cidr(crate::ip::Ipv4Address::new(127, 0, 0, 1), 8),
                scope: wire_route::SCOPE_HOST,
                broadcast: None,
                no_prefix_route: false,
                label: None,
            },
        )
        .unwrap();
    router.table.add_rule(Rule::new(
        valid_ipv4_cidr(crate::ip::Ipv4Address::UNSPECIFIED, 0).into(),
        Some(crate::ip::IpAddress::Ipv4(crate::ip::Ipv4Address::new(
            192, 168, 1, 1,
        ))),
        NetDeviceId::new(2).unwrap(),
        crate::ip::Ipv4Address::new(192, 168, 1, 2).into(),
    ));
    router
        .add_ipv4_addr(
            NetDeviceId::new(2).unwrap(),
            Ipv4AddrEntry {
                addr: valid_ipv4_cidr(crate::ip::Ipv4Address::new(192, 168, 1, 2), 24),
                scope: wire_route::SCOPE_UNIVERSE,
                broadcast: None,
                no_prefix_route: false,
                label: None,
            },
        )
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

fn addr_dump_payload() -> Vec<u8> {
    let mut payload = Vec::new();
    IfAddrMsg {
        family: wire_route::FAMILY_IPV4,
        prefix_len: 0,
        flags: 0,
        scope: 0,
        index: 0,
    }
    .write(&mut payload);
    payload
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

fn build_ipv4_addr_mutation_with_attrs(
    address: Ipv4Address,
    prefix_len: u8,
    seq: u32,
    flags: u16,
    extra_attrs: &[(u16, &[u8])],
) -> Vec<u8> {
    let mut payload = Vec::new();
    push_u8(&mut payload, wire_route::FAMILY_IPV4);
    push_u8(&mut payload, prefix_len);
    push_u8(&mut payload, 0);
    push_u8(&mut payload, wire_route::SCOPE_UNIVERSE);
    push_u32_ne(&mut payload, 2);
    payload.extend(attr(wire_addr::attr::LOCAL, &address.octets()));
    for (kind, value) in extra_attrs {
        payload.extend(attr(*kind, value));
    }
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
        RTM_DELNEIGH,
        // In-range messages with no handler still require CAP_NET_ADMIN
        // because their kind is not RTNL_KIND_GET.
        RTM_SETLINK,
    ] {
        assert!(rtnetlink_request_requires_privilege(msg_type));
    }
    for msg_type in [RTM_GETLINK, RTM_GETADDR, RTM_GETROUTE, RTM_GETNEIGH] {
        assert!(!rtnetlink_request_requires_privilege(msg_type));
    }
    for msg_type in [RTM_MAX + 1, u16::MAX] {
        assert!(!rtnetlink_request_requires_privilege(msg_type));
    }
}

#[def_test]
fn test_unknown_protocol_request_without_kernel_port_is_refused_without_response() {
    let socket = NetlinkSocket::new(i32::MAX, NetlinkSocketType::Raw);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 50, groups: 0 }))
        .unwrap();
    let mut request = build_nlmsg(u16::MAX, 61, NLM_F_REQUEST, Vec::new());
    let oversized_len = (request.len() as u32).checked_add(4).unwrap();
    request[..4].copy_from_slice(&oversized_len.to_ne_bytes());

    // An unregistered protocol has no kernel port 0.  A default-destination
    // datagram therefore fails as a missing unicast destination instead of
    // being silently consumed or answered with an empty response.
    assert_eq!(
        socket.send(Cursor::new(request.as_slice()), SendOptions::default()),
        Err(LinuxError::ECONNREFUSED.into())
    );
    assert!(socket.inner.rx_queue.lock().is_empty());
}

#[def_test(serial)]
fn test_unknown_protocol_rejects_group_subscription() {
    let socket = NetlinkSocket::new(i32::MAX, NetlinkSocketType::Raw);
    let request = SocketAddrEx::Netlink(NetlinkAddr { pid: 51, groups: 1 });

    // `netlink_bind` applies the generic nonroot-receive gate before it tries
    // to allocate a protocol group table.  Without privilege the gate fails
    // first; with privilege the missing table yields ENOENT.
    assert_eq!(
        socket.bind_with_cred(request.clone(), &Cred::new(1000, 1000)),
        Err(LinuxError::EPERM.into())
    );
    assert_eq!(
        socket.bind_with_cred(request, &Cred::root()),
        Err(LinuxError::ENOENT.into())
    );
    assert_socket_unbound(&socket);
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
fn test_newlink_resolves_names_and_honors_exclusive_replace_flags() {
    init_test_state();

    fn request(index: i32, flags: u16, attrs: &[(u16, &[u8])]) -> Vec<NetlinkPacket> {
        let mut payload = Vec::new();
        IfInfoMsg {
            family: 0,
            pad: 0,
            link_type: 0,
            index,
            flags: 0,
            change: 0,
        }
        .write(&mut payload);
        for (kind, data) in attrs {
            payload.extend(attr(*kind, data));
        }
        handle_rtnetlink_request(
            &build_nlmsg(RTM_NEWLINK, 70, NLM_F_REQUEST | NLM_F_ACK | flags, payload),
            0,
        )
    }

    let eth0 = [(wire_link::attr::IFNAME, b"eth0\0".as_slice())];
    let mtu_bytes = 1400u32.to_ne_bytes();
    let mtu = [(wire_link::attr::MTU, mtu_bytes.as_slice())];
    let mut attrs = eth0.to_vec();
    attrs.extend(mtu);

    // An existing device cannot be created exclusively or replaced.
    let exclusive = request(0, NLM_F_EXCL, &attrs);
    let [error] = exclusive.as_slice() else {
        panic!("expected EEXIST");
    };
    assert_eq!(
        read_i32_ne(&error.data, NLMSG_HDR_LEN),
        Some(-LinuxError::EEXIST.into_raw())
    );
    let replaced = request(0, NLM_F_REPLACE, &attrs);
    let [error] = replaced.as_slice() else {
        panic!("expected EOPNOTSUPP");
    };
    assert_eq!(
        read_i32_ne(&error.data, NLMSG_HDR_LEN),
        Some(-LinuxError::EOPNOTSUPP.into_raw())
    );

    // A zero index falls back to IFLA_IFNAME and updates the resolved device.
    assert_eq!(request(0, 0, &attrs).len(), 1);
    assert_eq!(
        SERVICE.link_snapshot_for_ifindex(2).expect("eth0").mtu,
        1400
    );

    // A negative index is invalid; a missing selector/device is ENODEV.
    let negative = request(-1, 0, &[]);
    let [error] = negative.as_slice() else {
        panic!("expected EINVAL");
    };
    assert_eq!(
        read_i32_ne(&error.data, NLMSG_HDR_LEN),
        Some(-LinuxError::EINVAL.into_raw())
    );
    let selector = request(0, 0, &[]);
    let [error] = selector.as_slice() else {
        panic!("expected ENODEV");
    };
    assert_eq!(
        read_i32_ne(&error.data, NLMSG_HDR_LEN),
        Some(-LinuxError::ENODEV.into_raw())
    );
    let missing = request(0, 0, &[(wire_link::attr::IFNAME, b"missing\0".as_slice())]);
    let [error] = missing.as_slice() else {
        panic!("expected ENODEV");
    };
    assert_eq!(
        read_i32_ne(&error.data, NLMSG_HDR_LEN),
        Some(-LinuxError::ENODEV.into_raw())
    );
}

#[def_test(serial)]
fn test_apply_link_update_rename_and_down() {
    init_test_state();
    let request = LinkUpdateRequest {
        index: 2,
        flags: 0,
        change: LINK_FLAG_UP,
        name: Some(String::from("ens3")),
        alt_name: None,
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
fn test_mtu_only_link_update_still_notifies() {
    init_test_state();
    let before = SERVICE.link_snapshot_for_ifindex(2).expect("eth0 link");
    let events = apply_link_update(LinkUpdateRequest {
        index: 2,
        flags: 0,
        change: 0,
        name: None,
        alt_name: None,
        mtu: Some(1400),
    })
    .unwrap();

    // Linux `rtmsg_ifinfo()` also reports non-flag state changes such as an
    // MTU update; `ifi_change` is the real flag delta, which is zero here.
    let link_event = events
        .iter()
        .find_map(|event| match event {
            NetworkEvent::LinkChanged {
                changed_flags,
                link,
            } => Some((*changed_flags, link)),
            _ => None,
        })
        .expect("MTU change publishes RTM_NEWLINK");
    assert_eq!(link_event.0, before.flags ^ link_event.1.flags);
    assert_eq!(link_event.1.mtu, 1400);
}

#[def_test(serial)]
fn test_link_up_reschedules_neighbor_timer() {
    init_test_state();
    apply_link_update(LinkUpdateRequest {
        index: 2,
        flags: 0,
        change: LINK_FLAG_UP,
        name: None,
        alt_name: None,
        mtu: None,
    })
    .unwrap();
    assert_eq!(
        SERVICE
            .link_snapshot_for_ifindex(2)
            .expect("eth0 link snapshot")
            .flags
            & LINK_FLAG_UP,
        0
    );

    let before = SERVICE.neighbor_timer_schedule_count();
    let events = apply_link_update(LinkUpdateRequest {
        index: 2,
        flags: LINK_FLAG_UP,
        change: LINK_FLAG_UP,
        name: None,
        alt_name: None,
        mtu: None,
    })
    .unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, NetworkEvent::LinkChanged { .. }))
    );
    // A down link produces no neighbor deadline, so the last poll cleared the
    // Service timer. The up transition must explicitly request a poll round
    // to restore the frozen deadlines.
    assert_eq!(SERVICE.neighbor_timer_schedule_count(), before + 1);
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
        alt_name: None,
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
        alt_name: None,
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
        alt_name: None,
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
        alt_name: None,
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
        alt_name: None,
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
        alt_name: None,
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
            alt_name: None,
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

    assert!(crate::unregister_netdev_for_test(kdevice::DeviceId::new(2)));
    assert_eq!(service.rebuild_count_for_tests(), rebuild_count + 1);
    assert_eq!(service.link_snapshots().len(), 1);
    assert_eq!(
        service.iface.lock().ip_addrs(),
        &[IpCidr::Ipv4(smoltcp::wire::Ipv4Cidr::new(
            Ipv4Address::new(127, 0, 0, 1),
            8,
        ))]
    );
    assert!(
        SERVICE
            .route_snapshot()
            .iter()
            .all(|route| route.dev != NetDeviceId::new(2).unwrap())
    );
    assert!(!SERVICE.has_neighbor(
        NetDeviceId::new(2).unwrap(),
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
                alt_name: None,
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
            alt_name: None,
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
            alt_name: None,
            mtu: None,
        }),
        Err(LinuxError::EEXIST)
    );
}

#[def_test(serial)]
fn test_link_dump_reads_device_snapshots() {
    init_test_state();
    let packets = handle_rtnetlink_request(
        &build_nlmsg(
            RTM_GETLINK,
            17,
            NLM_F_REQUEST | NLM_F_DUMP,
            link_dump_payload(&[]),
        ),
        0,
    );

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

fn link_dump_payload(attrs: &[u8]) -> Vec<u8> {
    let mut payload = Vec::new();
    IfInfoMsg {
        family: 0,
        pad: 0,
        link_type: 0,
        index: 0,
        flags: 0,
        change: 0,
    }
    .write(&mut payload);
    payload.extend_from_slice(attrs);
    payload
}

fn link_dump_messages(payload: Vec<u8>) -> Vec<(u16, u16, i32)> {
    handle_rtnetlink_request(
        &build_nlmsg(RTM_GETLINK, 21, NLM_F_REQUEST | NLM_F_DUMP, payload),
        0,
    )
    .into_iter()
    .filter_map(|packet| {
        let header = NlMsgHeader::read(&packet.data)?;
        Some((
            header.msg_type,
            header.flags,
            IfInfoMsg::read(&packet.data[NLMSG_HDR_LEN..])
                .ok()
                .map_or(0, |info| info.index),
        ))
    })
    .collect()
}

#[def_test(serial)]
fn test_link_dump_negative_master_filters_nothing_but_marks_output() {
    init_test_state();
    let messages = link_dump_messages(link_dump_payload(&attr(
        wire_link::attr::MASTER,
        &u32::MAX.to_ne_bytes(),
    )));

    assert_eq!(messages.len(), 3);
    assert_eq!(
        messages
            .iter()
            .filter(|(kind, ..)| *kind == RTM_NEWLINK)
            .map(|(_, _, index)| *index)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert!(
        messages
            .iter()
            .filter(|(kind, ..)| *kind == RTM_NEWLINK)
            .all(|(_, flags, _)| flags & NLM_F_DUMP_FILTERED != 0)
    );
    assert_eq!(messages.last().unwrap().0, NLMSG_DONE);
}

#[def_test(serial)]
fn test_link_dump_positive_master_filters_every_device() {
    init_test_state();
    let messages = link_dump_messages(link_dump_payload(&attr(
        wire_link::attr::MASTER,
        &2u32.to_ne_bytes(),
    )));

    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].0, NLMSG_DONE);
}

#[def_test(serial)]
fn test_link_dump_master_accepts_trailing_bytes() {
    init_test_state();
    let mut value = u32::MAX.to_ne_bytes().to_vec();
    value.extend_from_slice(&[0xaa; 4]);
    let messages = link_dump_messages(link_dump_payload(&attr(wire_link::attr::MASTER, &value)));

    assert_eq!(
        messages
            .iter()
            .filter(|(kind, ..)| *kind == RTM_NEWLINK)
            .count(),
        2
    );
    assert!(
        messages
            .iter()
            .filter(|(kind, ..)| *kind == RTM_NEWLINK)
            .all(|(_, flags, _)| flags & NLM_F_DUMP_FILTERED != 0)
    );
}

#[def_test(serial)]
fn test_link_dump_malformed_master_falls_back_to_unfiltered_walk() {
    init_test_state();
    let messages = link_dump_messages(link_dump_payload(&attr(wire_link::attr::MASTER, &[1, 0])));

    assert_eq!(
        messages
            .iter()
            .filter(|(kind, ..)| *kind == RTM_NEWLINK)
            .count(),
        2
    );
    assert!(
        messages
            .iter()
            .filter(|(kind, ..)| *kind == RTM_NEWLINK)
            .all(|(_, flags, _)| flags & NLM_F_DUMP_FILTERED == 0)
    );
}

#[def_test(serial)]
fn test_link_dump_keeps_filters_before_a_short_attribute() {
    init_test_state();
    let mut attrs = attr(wire_link::attr::MASTER, &u32::MAX.to_ne_bytes());
    // A zero-length attribute stops the liberal walk.  Linux keeps the
    // attributes already parsed, so the master filter remains active.
    push_u16_ne(&mut attrs, 0);
    push_u16_ne(&mut attrs, 0);
    let messages = link_dump_messages(link_dump_payload(&attrs));

    assert_eq!(
        messages
            .iter()
            .filter(|(kind, ..)| *kind == RTM_NEWLINK)
            .count(),
        2
    );
    assert!(
        messages
            .iter()
            .filter(|(kind, ..)| *kind == RTM_NEWLINK)
            .all(|(_, flags, _)| flags & NLM_F_DUMP_FILTERED != 0)
    );
}

#[def_test(serial)]
fn test_old_style_link_dump_parses_attributes_after_rtgenmsg() {
    init_test_state();
    // `rtgenmsg` is one byte on the wire, but `nlmsg_parse_deprecated` aligns
    // its header length to four bytes before looking for attributes.
    let mut payload = vec![0, 0, 0, 0];
    payload.extend_from_slice(&attr(wire_link::attr::MASTER, &u32::MAX.to_ne_bytes()));
    assert!(payload.len() < IfInfoMsg::SIZE);

    let messages = link_dump_messages(payload);
    assert_eq!(
        messages
            .iter()
            .filter(|(kind, ..)| *kind == RTM_NEWLINK)
            .count(),
        2
    );
    assert!(
        messages
            .iter()
            .filter(|(kind, ..)| *kind == RTM_NEWLINK)
            .all(|(_, flags, _)| flags & NLM_F_DUMP_FILTERED != 0)
    );
}

#[def_test(serial)]
fn test_link_dump_unknown_link_kind_does_not_filter() {
    init_test_state();
    let mut linkinfo = Vec::new();
    push_attr(&mut linkinfo, 1, b"does-not-exist\0");
    let messages = link_dump_messages(link_dump_payload(&attr(
        wire_link::attr::LINKINFO,
        &linkinfo,
    )));

    assert_eq!(
        messages
            .iter()
            .filter(|(kind, ..)| *kind == RTM_NEWLINK)
            .count(),
        2
    );
    assert!(
        messages
            .iter()
            .filter(|(kind, ..)| *kind == RTM_NEWLINK)
            .all(|(_, flags, _)| flags & NLM_F_DUMP_FILTERED == 0)
    );
}

#[def_test(serial)]
fn test_ack_payload_and_capped_flag_match_linux() {
    // A success acknowledgement always contains the fixed `nlmsgerr` header
    // and the original netlink header, but never the request payload.
    let request = build_nlmsg(RTM_GETLINK, 33, NLM_F_ACK, vec![0u8; 4]);
    let packets = handle_rtnetlink_request_with_cred(&request, 41, true, 0, false);
    let [ack] = packets.as_slice() else {
        panic!("expected one success acknowledgement");
    };
    assert_eq!(read_u16_ne(&ack.data, 4), Some(NLMSG_ERROR));
    assert_eq!(read_u16_ne(&ack.data, 6), Some(NLM_F_CAPPED));
    assert_eq!(read_i32_ne(&ack.data, NLMSG_HDR_LEN), Some(0));
    assert_eq!(
        read_u32_ne(&ack.data, 0),
        Some((NLMSG_HDR_LEN + 4 + NLMSG_HDR_LEN) as u32)
    );

    // A normal error echoes the whole original message and is not capped.
    let request = build_nlmsg(RTM_MAX + 1, 34, NLM_F_REQUEST, vec![0u8; 4]);
    let packets = handle_rtnetlink_request_with_cred(&request, 41, true, 0, false);
    let [error] = packets.as_slice() else {
        panic!("expected one error response");
    };
    assert_eq!(read_u16_ne(&error.data, 4), Some(NLMSG_ERROR));
    assert_eq!(read_u16_ne(&error.data, 6), Some(0));
    assert_eq!(
        read_i32_ne(&error.data, NLMSG_HDR_LEN),
        Some(-LinuxError::EOPNOTSUPP.into_raw())
    );
    assert_eq!(
        read_u32_ne(&error.data, 0),
        Some((NLMSG_HDR_LEN + 4 + request.len()) as u32)
    );

    // `NETLINK_CAP_ACK` omits the request echo from errors as well.
    let packets = handle_rtnetlink_request_with_cred(&request, 41, true, 0, true);
    let [error] = packets.as_slice() else {
        panic!("expected one capped error response");
    };
    assert_eq!(read_u16_ne(&error.data, 4), Some(NLMSG_ERROR));
    assert_eq!(read_u16_ne(&error.data, 6), Some(NLM_F_CAPPED));
    assert_eq!(
        read_u32_ne(&error.data, 0),
        Some((NLMSG_HDR_LEN + 4 + NLMSG_HDR_LEN) as u32)
    );
}

#[def_test(serial)]
fn test_netlink_cap_ack_option_round_trips() {
    let socket = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    let mut enabled = true;
    socket
        .get_option(GetSocketOption::NetlinkCapAck(&mut enabled))
        .unwrap();
    assert!(!enabled);

    socket
        .set_option(SetSocketOption::NetlinkCapAck(&true))
        .unwrap();
    socket
        .get_option(GetSocketOption::NetlinkCapAck(&mut enabled))
        .unwrap();
    assert!(enabled);

    socket
        .set_option(SetSocketOption::NetlinkCapAck(&false))
        .unwrap();
    socket
        .get_option(GetSocketOption::NetlinkCapAck(&mut enabled))
        .unwrap();
    assert!(!enabled);
}

#[def_test(serial)]
fn test_dump_bits_suppress_success_ack_even_when_requested() {
    // Shared flag values from the netlink header. They are request aliases of
    // `NLM_F_REPLACE` and `NLM_F_EXCL`, which matters on the reply side only.
    const NLM_F_ROOT: u16 = 0x0100;
    const NLM_F_MATCH: u16 = 0x0200;

    init_test_state();

    // Linux tests the combined `NLM_F_DUMP` mask for truthiness. Either of
    // its bits therefore selects a dump, not only the usual pair.
    for (dump_flags, bit) in [
        (NLM_F_DUMP, "NLM_F_DUMP"),
        (NLM_F_ROOT, "NLM_F_ROOT"),
        (NLM_F_MATCH, "NLM_F_MATCH"),
    ] {
        for (msg_type, seq, payload_len) in [
            (RTM_GETLINK, 90, IfInfoMsg::SIZE),
            (RTM_GETADDR, 91, IfAddrMsg::SIZE),
            (RTM_GETROUTE, 92, RtMsg::SIZE),
            (RTM_GETNEIGH, 93, NdMsg::SIZE),
        ] {
            let packets = handle_rtnetlink_request(
                &build_nlmsg(
                    msg_type,
                    seq,
                    NLM_F_REQUEST | dump_flags | NLM_F_ACK,
                    vec![0; payload_len],
                ),
                41,
            );

            let done = packets.last().expect("dump must contain a DONE trailer");
            assert_eq!(read_u16_ne(&done.data, 4), Some(NLMSG_DONE), "{bit}");
            assert_eq!(read_u32_ne(&done.data, 8), Some(seq), "{bit}");
            assert_eq!(read_u32_ne(&done.data, 12), Some(41), "{bit}");
            assert!(
                packets
                    .iter()
                    .all(|packet| read_u16_ne(&packet.data, 4) != Some(NLMSG_ERROR)),
                "successful dump {msg_type} with {bit} must not append an acknowledgement"
            );
        }
    }
}

#[def_test(serial)]
fn test_getlink_by_name_returns_only_matching_link() {
    init_test_state();
    let mut payload = Vec::new();
    IfInfoMsg {
        family: 0,
        pad: 0,
        link_type: 0,
        index: 0,
        flags: 0,
        change: 0,
    }
    .write(&mut payload);
    payload.extend(attr(wire_link::attr::IFNAME, b"eth0\0"));

    let packets =
        handle_rtnetlink_request(&build_nlmsg(RTM_GETLINK, 18, NLM_F_REQUEST, payload), 0);

    assert_eq!(packets.len(), 1);
    let header = NlMsgHeader::read(&packets[0].data).unwrap();
    let msg_type = header.msg_type;
    let flags = header.flags;
    assert_eq!(msg_type, RTM_NEWLINK);
    assert_eq!(flags & NLM_F_MULTI, 0);
    let info = IfInfoMsg::read(&packets[0].data[NLMSG_HDR_LEN..]).unwrap();
    let index = info.index;
    assert_eq!(index, 2);
}

#[def_test(serial)]
fn test_getlink_by_name_with_ack_appends_success_ack() {
    init_test_state();
    let mut payload = Vec::new();
    IfInfoMsg {
        family: 0,
        pad: 0,
        link_type: 0,
        index: 0,
        flags: 0,
        change: 0,
    }
    .write(&mut payload);
    payload.extend(attr(wire_link::attr::IFNAME, b"eth0\0"));

    let packets = handle_rtnetlink_request(
        &build_nlmsg(RTM_GETLINK, 18, NLM_F_REQUEST | NLM_F_ACK, payload),
        0,
    );

    assert_eq!(packets.len(), 2);
    assert_eq!(read_u16_ne(&packets[0].data, 4), Some(RTM_NEWLINK));
    assert_eq!(read_u16_ne(&packets[1].data, 4), Some(NLMSG_ERROR));
    assert_eq!(read_i32_ne(&packets[1].data, NLMSG_HDR_LEN), Some(0));
}

#[def_test(serial)]
fn test_getlink_non_strict_query_ignores_unknown_and_trailing_attributes() {
    init_test_state();
    let mut payload = Vec::new();
    IfInfoMsg {
        family: 0,
        pad: 0,
        link_type: 0,
        index: 0,
        flags: 0,
        change: 0,
    }
    .write(&mut payload);
    payload.extend(attr(wire_link::attr::IFNAME, b"eth0\0"));
    payload.extend(attr(9999, b"ignored"));
    // Linux's non-strict parser stops at a malformed trailing attribute
    // instead of failing the request.
    payload.extend([0xff, 0xff, 0xff]);

    let packets =
        handle_rtnetlink_request(&build_nlmsg(RTM_GETLINK, 19, NLM_F_REQUEST, payload), 0);
    let [reply] = packets.as_slice() else {
        panic!("unknown GETLINK attributes must be ignored");
    };
    let info = IfInfoMsg::read(&reply.data[NLMSG_HDR_LEN..]).unwrap();
    let index = info.index;
    assert_eq!(index, 2);

    // An unknown attribute is not a lookup key, so a query containing only
    // that attribute still fails the missing-selector check with `EINVAL`.
    let mut payload = Vec::new();
    IfInfoMsg {
        family: 0,
        pad: 0,
        link_type: 0,
        index: 0,
        flags: 0,
        change: 0,
    }
    .write(&mut payload);
    payload.extend(attr(9999, b"ignored"));
    let packets =
        handle_rtnetlink_request(&build_nlmsg(RTM_GETLINK, 20, NLM_F_REQUEST, payload), 0);
    let [error] = packets.as_slice() else {
        panic!("expected one GETLINK error");
    };
    assert_eq!(
        read_i32_ne(&error.data, NLMSG_HDR_LEN),
        Some(-LinuxError::EINVAL.into_raw())
    );
}

#[def_test(serial)]
fn test_getlink_alternate_name_is_only_a_fallback() {
    fn query_with(attrs: &[(u16, &[u8])]) -> Vec<NetlinkPacket> {
        init_test_state();
        let mut payload = Vec::new();
        IfInfoMsg {
            family: 0,
            pad: 0,
            link_type: 0,
            index: 0,
            flags: 0,
            change: 0,
        }
        .write(&mut payload);
        for (kind, data) in attrs {
            payload.extend(attr(*kind, data));
        }
        handle_rtnetlink_request(&build_nlmsg(RTM_GETLINK, 21, NLM_F_REQUEST, payload), 0)
    }

    // The project has no alternate-name registry, so an alternate-only
    // lookup reaches the device lookup and fails with `ENODEV` rather than
    // being rejected as an unsupported attribute.
    let packets = query_with(&[(wire_link::attr::ALT_IFNAME, b"alt-eth0\0")]);
    let [error] = packets.as_slice() else {
        panic!("expected one alternate-name lookup error");
    };
    assert_eq!(
        read_i32_ne(&error.data, NLMSG_HDR_LEN),
        Some(-LinuxError::ENODEV.into_raw())
    );

    // `IFLA_IFNAME` wins over an alternate name when both are present.
    let packets = query_with(&[
        (wire_link::attr::IFNAME, b"eth0\0"),
        (wire_link::attr::ALT_IFNAME, b"missing\0"),
    ]);
    let [reply] = packets.as_slice() else {
        panic!("expected one link reply");
    };
    let info = IfInfoMsg::read(&reply.data[NLMSG_HDR_LEN..]).unwrap();
    let index = info.index;
    assert_eq!(index, 2);

    // A failed primary name does not fall back to a valid alternate name.
    let packets = query_with(&[
        (wire_link::attr::IFNAME, b"missing\0"),
        (wire_link::attr::ALT_IFNAME, b"eth0\0"),
    ]);
    let [error] = packets.as_slice() else {
        panic!("expected one missing primary-name error");
    };
    assert_eq!(
        read_i32_ne(&error.data, NLMSG_HDR_LEN),
        Some(-LinuxError::ENODEV.into_raw())
    );
}

#[def_test(serial)]
fn test_getlink_rejects_netns_selectors_and_permanent_address() {
    for (kind, payload, expected) in [
        (
            wire_link::attr::NET_NS_PID,
            &[0u8; 4][..],
            LinuxError::EOPNOTSUPP,
        ),
        (
            wire_link::attr::NET_NS_FD,
            &[0u8; 4][..],
            LinuxError::EOPNOTSUPP,
        ),
        (
            wire_link::attr::TARGET_NETNSID,
            &[0u8; 4][..],
            LinuxError::EINVAL,
        ),
        (
            wire_link::attr::PERM_ADDRESS,
            &[0u8; 6][..],
            LinuxError::EINVAL,
        ),
    ] {
        init_test_state();
        let mut request = Vec::new();
        IfInfoMsg {
            family: 0,
            pad: 0,
            link_type: 0,
            index: 0,
            flags: 0,
            change: 0,
        }
        .write(&mut request);
        request.extend(attr(wire_link::attr::IFNAME, b"eth0\0"));
        request.extend(attr(kind, payload));
        let packets =
            handle_rtnetlink_request(&build_nlmsg(RTM_GETLINK, 22, NLM_F_REQUEST, request), 0);
        let [error] = packets.as_slice() else {
            panic!("expected one selector error for attribute {kind}");
        };
        assert_eq!(
            read_i32_ne(&error.data, NLMSG_HDR_LEN),
            Some(-expected.into_raw()),
            "attribute {kind}"
        );
    }
}

#[def_test(serial)]
fn test_getlink_validates_linkinfo_nested_payload() {
    fn query(linkinfo: &[u8]) -> Vec<NetlinkPacket> {
        init_test_state();
        let mut request = Vec::new();
        IfInfoMsg {
            family: 0,
            pad: 0,
            link_type: 0,
            index: 0,
            flags: 0,
            change: 0,
        }
        .write(&mut request);
        request.extend(attr(wire_link::attr::IFNAME, b"eth0\0"));
        request.extend(attr(wire_link::attr::LINKINFO, linkinfo));
        handle_rtnetlink_request(&build_nlmsg(RTM_GETLINK, 22, NLM_F_REQUEST, request), 0)
    }

    let malformed = query(&[0]);
    let [error] = malformed.as_slice() else {
        panic!("expected a malformed link-info error");
    };
    assert_eq!(
        read_i32_ne(&error.data, NLMSG_HDR_LEN),
        Some(-LinuxError::EINVAL.into_raw())
    );

    let empty = query(&[]);
    let [reply] = empty.as_slice() else {
        panic!("expected one link reply for empty link-info");
    };
    let index = IfInfoMsg::read(&reply.data[NLMSG_HDR_LEN..])
        .expect("valid link reply")
        .index;
    assert_eq!(index, 2);
}

#[def_test(serial)]
fn test_addr_dump_reads_router_addresses() {
    init_test_state();
    let packets = handle_rtnetlink_request(
        &build_nlmsg(
            RTM_GETADDR,
            18,
            NLM_F_REQUEST | NLM_F_DUMP,
            addr_dump_payload(),
        ),
        0,
    );
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
fn test_addr_dump_observes_ioctl_address_on_requested_interface() {
    init_test_state();
    let address = core::net::Ipv4Addr::new(192, 168, 1, 99);
    crate::set_interface_ipv4_addr("eth0", address, 24).unwrap();
    let mut payload = Vec::new();
    IfAddrMsg {
        family: 0,
        prefix_len: 0,
        flags: 0,
        scope: 0,
        index: 2,
    }
    .write(&mut payload);

    let packets = handle_rtnetlink_request(
        &build_nlmsg(RTM_GETADDR, 19, NLM_F_REQUEST | NLM_F_DUMP, payload),
        0,
    );
    let addresses: Vec<_> = packets
        .iter()
        .filter_map(|packet| {
            let header = NlMsgHeader::read(&packet.data)?;
            (header.msg_type == RTM_NEWADDR)
                .then(|| IfAddrMsg::read(&packet.data[NLMSG_HDR_LEN..]).unwrap())
        })
        .collect();

    // Non-strict address dumps ignore the requested interface index, so both
    // loopback and eth0 are reported even though the request names eth0.
    assert_eq!(addresses.len(), 2);
    assert_eq!(
        addresses.iter().map(|info| info.index).collect::<Vec<_>>(),
        vec![1, 2]
    );
    let eth0_packet = packets
        .iter()
        .find(|packet| {
            read_u16_ne(&packet.data, 4) == Some(RTM_NEWADDR)
                && IfAddrMsg::read(&packet.data[NLMSG_HDR_LEN..]).is_ok_and(|info| info.index == 2)
        })
        .unwrap();
    let attrs = parse_attrs(&eth0_packet.data[NLMSG_HDR_LEN + IfAddrMsg::SIZE..]).unwrap();
    let local = attrs
        .iter()
        .find(|attr| attr.kind == wire_addr::attr::LOCAL)
        .unwrap();
    assert_eq!(local.payload, address.octets().as_slice());
    assert_eq!(crate::interface_ipv4_addr("eth0"), Ok(address));

    let request = build_ipv4_addr_mutation(
        Ipv4Address::from_octets(address.octets()),
        20,
        NLM_F_REQUEST | NLM_F_ACK,
    );
    // Delete through rtnetlink and read the same state through the ioctl helper.
    let mut request = request;
    request[4..6].copy_from_slice(&RTM_DELADDR.to_ne_bytes());
    let response = handle_rtnetlink_request(&request, 0);
    assert_eq!(read_i32_ne(&response[0].data, NLMSG_HDR_LEN), Some(0));
    assert_eq!(
        crate::interface_ipv4_addr("eth0"),
        Err(LinuxError::EADDRNOTAVAIL)
    );
}

#[def_test(serial)]
fn test_newaddr_zero_local_is_a_silent_noop() {
    init_test_state();
    let before = SERVICE.ipv4_addr_entries().len();
    let request = AddrRequest {
        index: 2,
        family: wire_route::FAMILY_IPV4,
        prefix_len: 32,
        scope: wire_route::SCOPE_UNIVERSE,
        address: IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
        broadcast: None,
        flags: 0,
        label: None,
    };

    // Linux's `__inet_insert_ifa` returns before changing the address list or
    // generating a notification when the local address is zero.
    assert_eq!(add_addr(request, NLM_F_CREATE | NLM_F_EXCL), Ok(Vec::new()));
    assert_eq!(SERVICE.ipv4_addr_entries().len(), before);
    assert!(
        !SERVICE
            .ipv4_addr_entries()
            .iter()
            .any(|(_, entry)| { entry.addr.address() == crate::ip::Ipv4Address::new(0, 0, 0, 0) })
    );
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
        broadcast: None,
        flags: 0,
        label: None,
    };

    add_addr(req.clone(), NLM_F_CREATE | NLM_F_EXCL).unwrap();
    // An exact identity match without NLM_F_REPLACE is an error; Linux only
    // retains and republishes the existing entry for the replace request.
    assert_eq!(
        add_addr(req.clone(), NLM_F_CREATE | NLM_F_EXCL),
        Err(LinuxError::EEXIST)
    );
    assert!(
        SERVICE
            .ipv4_addr_entries()
            .iter()
            .any(|(dev, entry)| entry.addr.address()
                == crate::ip::Ipv4Address::new(192, 168, 1, 99)
                && *dev == NetDeviceId::new(2).unwrap())
    );

    let replaced = AddrRequest {
        scope: wire_route::SCOPE_HOST,
        ..req.clone()
    };
    add_addr(replaced.clone(), NLM_F_REPLACE).unwrap();
    assert!(
        SERVICE
            .ipv4_addr_entries()
            .iter()
            .any(
                |(_, entry)| entry.addr.address() == crate::ip::Ipv4Address::new(192, 168, 1, 99)
                    && entry.scope == wire_route::SCOPE_UNIVERSE
            )
    );

    // Replacing an exact match preserves the existing attributes, but Linux
    // still publishes RTM_NEWADDR for the retained entry.
    let events = add_addr(replaced.clone(), NLM_F_REPLACE).unwrap();
    assert_eq!(events.len(), 1);
    let NetworkEvent::AddrAdded { addr } = &events[0] else {
        panic!("replace must publish an address event");
    };
    assert_eq!(addr.dev, NetDeviceId::new(2).unwrap());
    assert_eq!(addr.entry.scope, wire_route::SCOPE_UNIVERSE);
    assert_eq!(
        addr.entry.addr,
        valid_ipv4_cidr(Ipv4Address::new(192, 168, 1, 99).into(), 24)
    );

    del_addr(DelAddrRequest {
        index: replaced.index,
        family: replaced.family,
        prefix_len: replaced.prefix_len,
        local: Some(replaced.address),
        address: None,
        label: None,
    })
    .unwrap();
    assert!(
        !SERVICE
            .ipv4_addr_entries()
            .iter()
            .any(|(dev, entry)| entry.addr.address()
                == crate::ip::Ipv4Address::new(192, 168, 1, 99)
                && *dev == NetDeviceId::new(2).unwrap())
    );
}

#[def_test(serial)]
fn test_route_parser_is_liberal_and_preserves_raw_protocol() {
    init_test_state();
    let route_payload = |protocol: u8, attrs: &[Vec<u8>]| {
        let mut payload = Vec::new();
        RtMsg {
            family: wire_route::FAMILY_IPV4,
            dst_len: 24,
            src_len: 0,
            tos: 0,
            table: 0,
            protocol,
            scope: wire_route::SCOPE_UNIVERSE,
            route_type: wire_route::TYPE_UNICAST,
            flags: 0,
        }
        .write(&mut payload);
        for item in attrs {
            payload.extend_from_slice(item);
        }
        payload
    };

    // Fixed-width attributes accept longer payloads and only the first four
    // bytes participate.  Attribute type flags are masked, unknown attributes
    // are ignored, and parsing stops at a malformed trailing attribute while
    // preserving everything before it.
    let mut trailing = attr(wire_route::attr::OIF, &[4, 0]);
    trailing.truncate(3);
    let mut attrs = vec![
        attr(wire_route::attr::DST, &[203, 0, 113, 77, 0xde, 0xad]),
        attr(wire_route::attr::OIF, &[3, 0, 0, 0, 0xaa]),
        attr(0x8000 | wire_route::attr::OIF, &[2, 0, 0, 0, 0xbb, 0xcc]),
        attr(wire_route::attr::TABLE, &[254, 0, 0, 0, 0xdd]),
        attr(99, &[1, 2, 3, 4]),
    ];
    attrs.push(trailing);
    let payload = route_payload(0, &attrs);
    let parsed = parse_route_request(&payload).unwrap();
    assert_eq!(parsed.protocol, 0);
    assert_eq!(parsed.table, 254);
    assert_eq!(parsed.oif, 2);
    assert_eq!(
        parsed.dst,
        Some(IpAddress::Ipv4(Ipv4Address::new(203, 0, 113, 77)))
    );
    assert_eq!(parsed.dst_len, 24);

    // A short fixed-width attribute is a policy failure, not a silently
    // ignored trailing fragment.
    let short_oif = route_payload(0, &[attr(wire_route::attr::OIF, &[1, 0, 0])]);
    assert_eq!(
        parse_route_request(&short_oif).unwrap_err(),
        LinuxError::ERANGE
    );

    // `rtm_protocol` is stored verbatim; there is no zero-to-BOOT conversion
    // in the netlink path.
    for protocol in [0, 78] {
        let payload = route_payload(
            protocol,
            &[attr(wire_route::attr::DST, &[203, 0, 113, protocol])],
        );
        let mut parsed = parse_route_request(&payload).unwrap();
        assert_eq!(parsed.protocol, protocol);
        // The liberal parser deliberately observed a non-main table above;
        // configured routes in this stack are restricted to the main table.
        parsed.table = wire_route::TABLE_MAIN;
        parsed.dst_len = 32;
        parsed.oif = 2;
        validate_route_request(parsed).unwrap();
        let mut events = add_route(parsed, NLM_F_CREATE | NLM_F_EXCL).unwrap();
        let route = events
            .drain(..)
            .find_map(|event| match event {
                NetworkEvent::RouteAdded { route, .. } => Some(route),
                _ => None,
            })
            .expect("configured route event");
        assert_eq!(route.protocol, protocol);
    }

    // Linux rejects ECN bits in `rtm_tos` and out-of-range route types on
    // every route operation.  `fib_create_info()` derives `RT_SCOPE_NOWHERE`
    // for `RTN_UNSPEC`, so an unspecified type is not insertable in any scope.
    // That rule and the dead/link-down checks are insertion-only:
    // RTM_DELROUTE accepts those same fields to identify an existing route.
    let base = parse_route_request(&route_payload(
        0,
        &[attr(wire_route::attr::DST, &[198, 51, 100, 0])],
    ))
    .unwrap();
    for request in [
        RouteRequest { tos: 3, ..base },
        RouteRequest {
            route_type: wire_route::TYPE_MAX + 1,
            ..base
        },
    ] {
        assert_eq!(validate_route_request(request), Err(LinuxError::EINVAL));
        assert_eq!(
            validate_route_insert_request(request),
            Err(LinuxError::EINVAL)
        );
    }
    for request in [
        RouteRequest {
            route_type: 0,
            scope: wire_route::SCOPE_UNIVERSE,
            ..base
        },
        RouteRequest {
            route_type: 0,
            scope: wire_route::SCOPE_LINK,
            ..base
        },
        RouteRequest {
            route_type: 0,
            scope: wire_route::SCOPE_NOWHERE,
            ..base
        },
        RouteRequest {
            flags: wire_route::RTNH_F_DEAD,
            ..base
        },
        RouteRequest {
            flags: wire_route::RTNH_F_LINKDOWN,
            ..base
        },
    ] {
        assert_eq!(validate_route_request(request), Ok(()));
        assert_eq!(
            validate_route_insert_request(request),
            Err(LinuxError::EINVAL)
        );
    }
}

#[def_test(serial)]
fn test_newaddr_label_policy_and_raw_byte_round_trip() {
    init_test_state();
    let request_flags = NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL;
    let request_errno = |request: &[u8]| {
        let packets = handle_rtnetlink_request(request, 0);
        let [error] = packets.as_slice() else {
            panic!("expected one address response");
        };
        read_i32_ne(&error.data, NLMSG_HDR_LEN).unwrap()
    };

    // Linux NLA_STRING validation rejects an empty payload, a 16-byte string
    // without a trailing NUL, and a 16-byte payload whose NUL is not last.
    for label in [Vec::new(), vec![b'x'; 16], {
        let mut label = vec![b'x'; 16];
        label[3] = 0;
        label
    }] {
        let request = build_ipv4_addr_mutation_with_attrs(
            Ipv4Address::new(192, 168, 30, 81),
            24,
            81,
            request_flags,
            &[(wire_addr::attr::LABEL, &label)],
        );
        assert_eq!(
            request_errno(&request),
            -LinuxError::ERANGE.into_raw(),
            "label {label:?}"
        );
    }

    // Labels are raw bytes, not UTF-8.  A 15-byte label without a trailing NUL
    // is accepted and is reported as a NUL-terminated string.
    let raw_label = [
        0x66, 0x80, 0xff, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c,
    ];
    let request = build_ipv4_addr_mutation_with_attrs(
        Ipv4Address::new(192, 168, 30, 81),
        24,
        82,
        request_flags,
        &[(wire_addr::attr::LABEL, &raw_label)],
    );
    assert_eq!(request_errno(&request), 0);

    // A 16-byte payload with exactly one trailing NUL is within policy.
    let mut label_with_nul = vec![b'z'; 15];
    label_with_nul.push(0);
    let request = build_ipv4_addr_mutation_with_attrs(
        Ipv4Address::new(192, 168, 30, 82),
        24,
        83,
        request_flags,
        &[(wire_addr::attr::LABEL, &label_with_nul)],
    );
    assert_eq!(request_errno(&request), 0);

    // An explicit empty label is legal but Linux omits `IFA_LABEL` entirely
    // because `inet_fill_ifaddr()` tests `ifa_label[0]` before reporting it.
    let request = build_ipv4_addr_mutation_with_attrs(
        Ipv4Address::new(192, 168, 30, 83),
        24,
        84,
        request_flags,
        &[(wire_addr::attr::LABEL, &[0])],
    );
    assert_eq!(request_errno(&request), 0);

    let packets = handle_rtnetlink_request(
        &build_nlmsg(
            RTM_GETADDR,
            85,
            NLM_F_REQUEST | NLM_F_DUMP,
            addr_dump_payload(),
        ),
        0,
    );
    let label_for = |address: [u8; 4]| -> Option<Option<Vec<u8>>> {
        packets.iter().find_map(|packet| {
            let payload = &packet.data[NLMSG_HDR_LEN..];
            let attrs = parse_attrs(payload.get(IfAddrMsg::SIZE..)?).ok()?;
            let local = attrs
                .iter()
                .find(|attr| attr.kind == wire_addr::attr::LOCAL)?;
            if local.payload != address {
                return None;
            }
            Some(
                attrs
                    .iter()
                    .find(|attr| attr.kind == wire_addr::attr::LABEL)
                    .map(|attr| attr.payload.to_vec()),
            )
        })
    };

    let mut expected_raw = raw_label.to_vec();
    expected_raw.push(0);
    assert_eq!(label_for([192, 168, 30, 81]), Some(Some(expected_raw)));
    assert_eq!(
        label_for([192, 168, 30, 82]),
        Some(Some(label_with_nul.clone()))
    );
    assert_eq!(label_for([192, 168, 30, 83]), Some(None));

    // `nla_strcmp()` compares the parsed label, so the raw NUL-terminated
    // value selects exactly that address.  A mismatched label removes nothing.
    let mut padded = [0u8; 16];
    padded[..raw_label.len()].copy_from_slice(&raw_label);
    assert_eq!(
        del_addr(DelAddrRequest {
            index: 2,
            family: wire_route::FAMILY_IPV4,
            prefix_len: 0,
            local: None,
            address: None,
            label: Some([b'n'; 16]),
        }),
        Err(LinuxError::EADDRNOTAVAIL)
    );
    del_addr(DelAddrRequest {
        index: 2,
        family: wire_route::FAMILY_IPV4,
        prefix_len: 0,
        local: None,
        address: None,
        label: Some(padded),
    })
    .unwrap();
    assert!(
        !SERVICE
            .ipv4_addr_entries()
            .iter()
            .any(|(_, entry)| entry.addr.address() == crate::ip::Ipv4Address::new(192, 168, 30, 81))
    );
}

#[def_test(serial)]
fn test_newaddr_derives_secondary_from_address_ordering() {
    init_test_state();
    let request = |address: Ipv4Address, prefix_len, flags| AddrRequest {
        index: 2,
        family: wire_route::FAMILY_IPV4,
        prefix_len,
        scope: wire_route::SCOPE_UNIVERSE,
        address: IpAddress::Ipv4(address),
        broadcast: None,
        flags,
        label: None,
    };

    let primary = Ipv4Address::new(192, 168, 40, 1);
    let secondary = Ipv4Address::new(192, 168, 40, 2);
    let events = add_addr(request(primary, 24, 0), NLM_F_CREATE | NLM_F_EXCL).unwrap();
    let NetworkEvent::AddrAdded { addr } = &events[0] else {
        panic!("primary must publish an address event");
    };
    assert!(!addr.secondary);

    let events = add_addr(request(secondary, 24, 0), NLM_F_CREATE | NLM_F_EXCL).unwrap();
    let NetworkEvent::AddrAdded { addr } = &events[0] else {
        panic!("secondary must publish an address event");
    };
    assert!(addr.secondary);
    let prefix_routes = SERVICE
        .route_snapshot()
        .iter()
        .filter(|route| {
            route.filter.prefix_len() == 24
                && route.filter.address()
                    == crate::ip::IpAddress::Ipv4(crate::ip::Ipv4Address::new(192, 168, 40, 0))
        })
        .count();
    // A secondary shares the primary's connected prefix route rather than
    // installing a duplicate.
    assert_eq!(prefix_routes, 1);

    // An incoming SECONDARY bit is not authoritative.  A different network is
    // a primary address even when the request claims otherwise.
    let primary_other_network = Ipv4Address::new(192, 168, 41, 2);
    let events = add_addr(
        request(primary_other_network, 16, wire_addr::FLAG_SECONDARY),
        NLM_F_CREATE | NLM_F_EXCL,
    )
    .unwrap();
    let NetworkEvent::AddrAdded { addr } = &events[0] else {
        panic!("address must publish an address event");
    };
    assert!(!addr.secondary);

    let packets = handle_rtnetlink_request(
        &build_nlmsg(
            RTM_GETADDR,
            86,
            NLM_F_REQUEST | NLM_F_DUMP,
            addr_dump_payload(),
        ),
        0,
    );
    let secondary_flags = packets.iter().find_map(|packet| {
        let payload = &packet.data[NLMSG_HDR_LEN..];
        let attrs = parse_attrs(&payload[IfAddrMsg::SIZE..]).ok()?;
        let local = attrs
            .iter()
            .find(|attr| attr.kind == wire_addr::attr::LOCAL)?;
        (local.payload == secondary.octets()).then(|| {
            attrs
                .iter()
                .find(|attr| attr.kind == wire_addr::attr::FLAGS)
                .map(|attr| u32::from_ne_bytes(attr.payload[..4].try_into().unwrap()))
                .unwrap()
        })
    });
    assert_eq!(
        secondary_flags,
        Some(u32::from(wire_addr::FLAG_PERMANENT) | wire_addr::FLAG_SECONDARY)
    );
}

#[def_test(serial)]
fn test_deleting_primary_removes_secondaries_before_primary() {
    init_test_state();
    let request = |address: Ipv4Address, broadcast: Option<Ipv4Address>| AddrRequest {
        index: 2,
        family: wire_route::FAMILY_IPV4,
        prefix_len: 24,
        scope: wire_route::SCOPE_UNIVERSE,
        address: IpAddress::Ipv4(address),
        broadcast: broadcast.map(IpAddress::Ipv4),
        flags: 0,
        label: None,
    };
    let primary = Ipv4Address::new(192, 168, 50, 1);
    let first_secondary = Ipv4Address::new(192, 168, 50, 2);
    let second_secondary = Ipv4Address::new(192, 168, 50, 3);
    let first_secondary_broadcast = Ipv4Address::new(192, 168, 50, 254);
    let second_secondary_broadcast = Ipv4Address::new(192, 168, 50, 253);
    for (address, broadcast) in [
        (primary, None),
        (first_secondary, Some(first_secondary_broadcast)),
        (second_secondary, Some(second_secondary_broadcast)),
    ] {
        add_addr(request(address, broadcast), NLM_F_CREATE | NLM_F_EXCL).unwrap();
    }

    let events = del_addr(DelAddrRequest {
        index: 2,
        family: wire_route::FAMILY_IPV4,
        prefix_len: 0,
        local: Some(IpAddress::Ipv4(primary)),
        address: None,
        label: None,
    })
    .unwrap();

    let removed_addrs: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            NetworkEvent::AddrRemoved { addr } => Some(addr.entry.addr.address()),
            _ => None,
        })
        .collect();
    assert_eq!(
        removed_addrs,
        vec![
            first_secondary.into(),
            second_secondary.into(),
            primary.into()
        ]
    );
    // Linux removes the address, then runs the FIB notifier for that exact
    // address, before moving to the next one.  Every route notification must
    // therefore be attributable to the most recently removed address, and the
    // primary's routes must follow `fib_del_ifaddr()` order: connected prefix,
    // computed broadcast, then local host route.
    let mut current = None;
    let mut route_filters: Vec<Vec<crate::ip::IpCidr>> = Vec::new();
    for event in &events {
        match event {
            NetworkEvent::AddrRemoved { addr } => {
                current = Some(addr);
                route_filters.push(Vec::new());
            }
            NetworkEvent::RouteRemoved { route, .. } => {
                let addr = current.expect("route removal must follow an address removal");
                assert!(
                    route.is_derived_from_address(addr.dev, addr.entry),
                    "route {:?} does not belong to the preceding address {:?}",
                    route,
                    addr.entry.addr,
                );
                route_filters
                    .last_mut()
                    .expect("current address")
                    .push(route.filter);
            }
            other => panic!("unexpected address-transition event: {other:?}"),
        }
    }
    let host = |address: Ipv4Address| -> crate::ip::IpCidr {
        crate::ip::Ipv4Cidr::new(address.into(), 32).unwrap().into()
    };
    assert_eq!(
        route_filters,
        vec![
            vec![host(first_secondary_broadcast), host(first_secondary)],
            vec![host(second_secondary_broadcast), host(second_secondary)],
            vec![
                crate::ip::Ipv4Cidr::new(Ipv4Address::new(192, 168, 50, 0).into(), 24)
                    .unwrap()
                    .into(),
                host(Ipv4Address::new(192, 168, 50, 255)),
                host(primary),
            ],
        ]
    );
}

#[def_test(serial)]
fn test_deleting_last_address_owner_removes_configured_prefsrc_route() {
    init_test_state();
    let address = Ipv4Address::new(192, 168, 1, 2);
    del_addr(DelAddrRequest {
        index: 2,
        family: wire_route::FAMILY_IPV4,
        prefix_len: 24,
        local: Some(IpAddress::Ipv4(address)),
        address: None,
        label: None,
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
        tos: 0,
        flags: 0,
        oif: 2,
        dst: Some(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 0))),
        src: None,
        gateway: Some(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 3))),
        prefsrc: Some(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 2))),
    };

    assert_eq!(
        add_route(req, NLM_F_CREATE | NLM_F_EXCL),
        Err(LinuxError::EEXIST)
    );
    // SIOCADDRT allows a configured route alongside the connected route.
    crate::control::add_ipv4_route(
        Some(core::net::Ipv4Addr::new(192, 168, 1, 0)),
        24,
        Some(core::net::Ipv4Addr::new(192, 168, 1, 1)),
        Some("eth0"),
    )
    .unwrap();
    add_route(req, NLM_F_REPLACE).unwrap();
    assert!(SERVICE.route_snapshot().iter().any(|route| {
        route.filter.prefix_len() == req.dst_len
            && route.via == Some(crate::ip::Ipv4Address::new(192, 168, 1, 3).into())
    }));

    // `fib_table_delete()` matches the same key without applying the
    // insertion-only checks for RTN_UNSPEC/universe and dead/link-down flags.
    let delete_with_insert_invalid_fields = RouteRequest {
        route_type: 0,
        flags: wire_route::RTNH_F_DEAD | wire_route::RTNH_F_LINKDOWN,
        ..req
    };
    del_route(delete_with_insert_invalid_fields).unwrap();
    assert!(
        !SERVICE
            .route_snapshot()
            .iter()
            .any(|route| route.filter.prefix_len() == req.dst_len && route.via.is_some())
    );
}

#[def_test(serial)]
fn test_exact_route_replace_publishes_no_notification() {
    init_test_state();
    let req = RouteRequest {
        family: wire_route::FAMILY_IPV4,
        dst_len: 24,
        src_len: 0,
        table: RT_TABLE_MAIN,
        protocol: wire_route::PROTOCOL_BOOT,
        scope: wire_route::SCOPE_UNIVERSE,
        route_type: RTN_UNICAST,
        tos: 0,
        flags: 0,
        oif: 2,
        dst: Some(IpAddress::Ipv4(Ipv4Address::new(172, 30, 0, 0))),
        src: None,
        gateway: Some(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 3))),
        prefsrc: Some(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 2))),
    };

    let events = add_route(req, NLM_F_CREATE | NLM_F_EXCL).unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, NetworkEvent::RouteAdded { .. }))
    );
    // `fib_table_insert` skips every notification when the replacement target
    // is identical to the existing route, even though the request succeeds.
    assert!(add_route(req, NLM_F_REPLACE).unwrap().is_empty());
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

    let packets = handle_rtnetlink_request(
        &build_nlmsg(
            RTM_NEWROUTE,
            21,
            NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
            payload,
        ),
        0,
    );

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
        NetDeviceId::new(2).unwrap(),
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
    // Replacing an entry with the same link-layer state is a no-op and must
    // not produce a kernel notification.
    assert!(add_neigh(replaced, NLM_F_REPLACE).unwrap().is_empty());
    assert!(SERVICE.has_neighbor(
        NetDeviceId::new(2).unwrap(),
        crate::ip::IpAddress::Ipv4(match replaced.dst {
            IpAddress::Ipv4(address) => address.into(),
            IpAddress::Ipv6(_) => panic!("IPv4 test neighbor"),
        }),
    ));

    // NLM_F_EXCL is the only flag that makes an existing entry an error.
    // Without NLM_F_REPLACE, Linux also clears the override bits for an
    // existing entry, so a different lladdr is silently ignored.
    assert!(add_neigh(req, NLM_F_CREATE).unwrap().is_empty());
    let removed = del_neigh(req).unwrap();
    let [NetworkEvent::NeighborRemoved { neighbor }] = removed.as_slice() else {
        panic!("expected one neighbor removal event");
    };
    assert_eq!(
        neighbor.state,
        NeighborState::Permanent {
            hardware_addr: [1, 2, 3, 4, 5, 6],
        }
    );

    let missing = NeighRequest {
        dst: IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 11)),
        ..req
    };
    assert_eq!(add_neigh(missing, NLM_F_REPLACE), Err(LinuxError::ENOENT));
}

fn raw_newneigh_request(
    state: u16,
    flags: u16,
    dst: [u8; 4],
    lladdr: Option<[u8; 6]>,
    seq: u32,
) -> Vec<u8> {
    let mut payload = neigh_request_payload(wire_route::FAMILY_IPV4, 2, state, 0, 0, None);
    payload.extend(attr(wire_neigh::attr::DST, &dst));
    if let Some(lladdr) = lladdr {
        payload.extend(attr(wire_neigh::attr::LLADDR, &lladdr));
    }
    build_nlmsg(
        RTM_NEWNEIGH,
        seq,
        NLM_F_REQUEST | NLM_F_ACK | flags,
        payload,
    )
}

#[def_test(serial)]
fn test_newneigh_ignores_lladdr_when_no_interface_is_named() {
    init_test_state();
    let mut payload = neigh_request_payload(
        wire_route::FAMILY_IPV6,
        0,
        wire_neigh::STATE_PERMANENT,
        0,
        0,
        None,
    );
    payload.extend(attr(wire_neigh::attr::DST, &[192, 168, 1, 42]));
    payload.extend(attr(wire_neigh::attr::LLADDR, &[1, 2]));
    let request = build_nlmsg(
        RTM_NEWNEIGH,
        75,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE,
        payload,
    );

    // Linux skips NDA_LLADDR validation when ndm_ifindex is zero, so the
    // unsupported family is reported first.
    assert_neigh_mutation_result(&request, Err(LinuxError::EAFNOSUPPORT));
}

#[def_test(serial)]
fn test_admin_incomplete_neighbor_schedules_poll_timer() {
    init_test_state();
    let before = SERVICE.neighbor_timer_schedule_count();
    let dst = IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 41));

    // Installing an administratively created INCOMPLETE neighbor must arm the
    // network poller.  On an idle link the last poll round recorded no
    // aggregate deadline, so a wait-set wake alone would never run the first
    // solicitation or its retransmit deadlines.
    let events = add_neigh(
        NeighRequest {
            family: wire_route::FAMILY_IPV4,
            ifindex: 2,
            state: wire_neigh::STATE_INCOMPLETE,
            flags: 0,
            dst,
            lladdr: None,
        },
        NLM_F_CREATE,
    )
    .unwrap();
    assert!(matches!(
        events.as_slice(),
        [NetworkEvent::NeighborAdded {
            neighbor: NeighborUpdate {
                state: NeighborState::Incomplete,
                ..
            }
        }]
    ));
    assert_eq!(SERVICE.neighbor_timer_schedule_count(), before + 1);
}

#[def_test(serial)]
fn test_newneigh_existence_checks_precede_state_and_lladdr_validation() {
    init_test_state();
    let dst = [192, 168, 1, 40];
    let lladdr = [0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0xee];
    let dev = NetDeviceId::new(2).unwrap();
    let destination = kernel_ipv4(Ipv4Address::new(dst[0], dst[1], dst[2], dst[3]));
    add_neigh(
        NeighRequest {
            family: wire_route::FAMILY_IPV4,
            ifindex: 2,
            state: wire_neigh::STATE_PERMANENT,
            flags: 0,
            dst: IpAddress::Ipv4(Ipv4Address::new(dst[0], dst[1], dst[2], dst[3])),
            lladdr: Some(lladdr),
        },
        NLM_F_CREATE | NLM_F_EXCL,
    )
    .unwrap();

    // Linux checks `NLM_F_EXCL` before validating whether the requested
    // neighbor state or link-layer address can be represented.
    assert_neigh_mutation_result(
        &raw_newneigh_request(
            wire_neigh::STATE_REACHABLE,
            NLM_F_CREATE | NLM_F_EXCL,
            dst,
            None,
            72,
        ),
        Err(LinuxError::EEXIST),
    );

    // A missing entry without `NLM_F_CREATE` fails before state validation.
    assert_neigh_mutation_result(
        &raw_newneigh_request(wire_neigh::STATE_REACHABLE, 0, [192, 168, 1, 41], None, 73),
        Err(LinuxError::ENOENT),
    );

    // Updating an existing permanent entry without NDA_LLADDR reuses the
    // stored hardware address.  Without NLM_F_REPLACE the update is a no-op.
    assert_neigh_mutation_result(
        &raw_newneigh_request(wire_neigh::STATE_PERMANENT, NLM_F_CREATE, dst, None, 74),
        Ok(()),
    );
    let snapshot = SERVICE
        .neighbor_snapshot(dev, destination)
        .expect("permanent neighbor retained");
    assert_eq!(
        snapshot.state,
        NeighborState::Permanent {
            hardware_addr: lladdr,
        }
    );
}

#[def_test(serial)]
fn test_newneigh_matches_liberal_linux_attribute_rules() {
    init_test_state();
    let dev = NetDeviceId::new(2).unwrap();
    let dst = Ipv4Address::new(192, 168, 1, 90);
    let lladdr = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];

    // `nla_type()` masks NLA_F_NESTED even for base neighbor attributes, and
    // `neigh_add` treats the destination and link address as a key-length
    // prefix rather than requiring an exact payload width.
    let tagged_dst = attr(wire_neigh::attr::DST | 0x8000, &dst.octets());
    let tagged_lladdr = attr(wire_neigh::attr::LLADDR | 0x8000, &lladdr);
    let request = neigh_mutation_request(
        RTM_NEWNEIGH,
        wire_neigh::STATE_PERMANENT,
        [tagged_dst, tagged_lladdr].concat(),
        &[],
        71,
    );
    assert_neigh_mutation_result(&request, Ok(()));
    assert_eq!(
        SERVICE
            .neighbor_snapshot(dev, kernel_ipv4(dst))
            .map(|neighbor| neighbor.state),
        Some(NeighborState::Permanent {
            hardware_addr: lladdr,
        })
    );

    let long_dst = Ipv4Address::new(192, 168, 1, 91);
    let mut long_dst_payload = long_dst.octets().to_vec();
    long_dst_payload.extend_from_slice(&[0xde, 0xad]);
    let mut long_lladdr_payload = lladdr.to_vec();
    long_lladdr_payload.extend_from_slice(&[0xbe, 0xef]);
    let request = neigh_mutation_request(
        RTM_NEWNEIGH,
        wire_neigh::STATE_PERMANENT,
        [
            attr(wire_neigh::attr::DST, &long_dst_payload),
            attr(wire_neigh::attr::LLADDR, &long_lladdr_payload),
            // Unknown attributes are skipped, and so is attribute type zero.
            attr(99, &[1, 2, 3]),
            attr(0, &[]),
        ]
        .concat(),
        // Liberal parsing accepts a final fragment shorter than one attribute.
        &[0, 0],
        72,
    );
    assert_neigh_mutation_result(&request, Ok(()));
    assert_eq!(
        SERVICE
            .neighbor_snapshot(dev, kernel_ipv4(long_dst))
            .map(|neighbor| neighbor.state),
        Some(NeighborState::Permanent {
            hardware_addr: lladdr,
        })
    );

    // A known attribute that violates `nda_policy` aborts the update, while
    // malformed trailing data alone does not.
    let rejected_dst = Ipv4Address::new(192, 168, 1, 92);
    let request = neigh_mutation_request(
        RTM_NEWNEIGH,
        wire_neigh::STATE_PERMANENT,
        [
            attr(wire_neigh::attr::DST, &rejected_dst.octets()),
            attr(wire_neigh::attr::LLADDR, &lladdr),
            attr(wire_neigh::attr::CACHEINFO, &[0; 4]),
        ]
        .concat(),
        &[],
        73,
    );
    assert_neigh_mutation_result(&request, Err(LinuxError::EINVAL));
    assert!(
        SERVICE
            .neighbor_snapshot(dev, kernel_ipv4(rejected_dst))
            .is_none()
    );

    // NUD_NONE is not silently promoted to NUD_PERMANENT.
    let none_dst = Ipv4Address::new(192, 168, 1, 93);
    let request = neigh_mutation_request(
        RTM_NEWNEIGH,
        0,
        [
            attr(wire_neigh::attr::DST, &none_dst.octets()),
            attr(wire_neigh::attr::LLADDR, &lladdr),
        ]
        .concat(),
        &[],
        74,
    );
    assert_neigh_mutation_result(&request, Err(LinuxError::EOPNOTSUPP));
    assert!(
        SERVICE
            .neighbor_snapshot(dev, kernel_ipv4(none_dst))
            .is_none()
    );
}

#[def_test(serial)]
fn test_neigh_mutation_error_precedence_matches_linux() {
    init_test_state();
    let dst4 = [192, 168, 1, 200];
    let dst6 = [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
    let lladdr = [0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0xee];

    let ipv4_attrs = || {
        [
            attr(wire_neigh::attr::DST, &dst4),
            attr(wire_neigh::attr::LLADDR, &lladdr),
        ]
        .concat()
    };
    let ipv6_attrs = || {
        [
            attr(wire_neigh::attr::DST, &dst6),
            attr(wire_neigh::attr::LLADDR, &lladdr),
        ]
        .concat()
    };

    // `neigh_add` resolves a nonzero interface before examining NDA_LLADDR,
    // ndm_family, and NDA_DST.
    let request = build_neigh_mutation(
        RTM_NEWNEIGH,
        wire_route::FAMILY_IPV6,
        9999,
        ipv6_attrs(),
        80,
    );
    assert_neigh_mutation_result(&request, Err(LinuxError::ENODEV));

    let mut short_lladdr = attr(wire_neigh::attr::DST, &dst4);
    short_lladdr.extend(attr(wire_neigh::attr::LLADDR, &lladdr[..3]));
    let request = build_neigh_mutation(
        RTM_NEWNEIGH,
        wire_route::FAMILY_IPV4,
        9999,
        short_lladdr,
        81,
    );
    assert_neigh_mutation_result(&request, Err(LinuxError::ENODEV));

    let request = build_neigh_mutation(RTM_NEWNEIGH, wire_route::FAMILY_IPV6, 2, ipv6_attrs(), 82);
    assert_neigh_mutation_result(&request, Err(LinuxError::EAFNOSUPPORT));

    let mut short_lladdr = attr(wire_neigh::attr::DST, &dst4);
    short_lladdr.extend(attr(wire_neigh::attr::LLADDR, &lladdr[..3]));
    let request = build_neigh_mutation(RTM_NEWNEIGH, wire_route::FAMILY_IPV4, 2, short_lladdr, 83);
    assert_neigh_mutation_result(&request, Err(LinuxError::EINVAL));

    let request = build_neigh_mutation(RTM_NEWNEIGH, wire_route::FAMILY_IPV6, 0, ipv6_attrs(), 84);
    assert_neigh_mutation_result(&request, Err(LinuxError::EAFNOSUPPORT));

    let request = build_neigh_mutation(RTM_NEWNEIGH, wire_route::FAMILY_IPV4, 0, ipv4_attrs(), 85);
    assert_neigh_mutation_result(&request, Err(LinuxError::EINVAL));

    // Attribute policy and the mandatory destination still run before the
    // interface lookup, exactly as `nlmsg_parse_deprecated` does.
    let request = build_neigh_mutation(
        RTM_NEWNEIGH,
        wire_route::FAMILY_IPV4,
        9999,
        attr(wire_neigh::attr::LLADDR, &lladdr),
        86,
    );
    assert_neigh_mutation_result(&request, Err(LinuxError::EINVAL));

    // `neigh_delete` only locates NDA_DST, then resolves the interface, then
    // the family, then the destination length.
    let delete_attrs4 = || attr(wire_neigh::attr::DST, &dst4);
    let delete_attrs6 = || attr(wire_neigh::attr::DST, &dst6);

    let request = build_neigh_mutation(
        RTM_DELNEIGH,
        wire_route::FAMILY_IPV6,
        9999,
        delete_attrs6(),
        87,
    );
    assert_neigh_mutation_result(&request, Err(LinuxError::ENODEV));

    let request = build_neigh_mutation(
        RTM_DELNEIGH,
        wire_route::FAMILY_IPV4,
        9999,
        attr(wire_neigh::attr::DST, &dst4[..2]),
        88,
    );
    assert_neigh_mutation_result(&request, Err(LinuxError::ENODEV));

    let request = build_neigh_mutation(
        RTM_DELNEIGH,
        wire_route::FAMILY_IPV6,
        2,
        delete_attrs6(),
        89,
    );
    assert_neigh_mutation_result(&request, Err(LinuxError::EAFNOSUPPORT));

    let request = build_neigh_mutation(
        RTM_DELNEIGH,
        wire_route::FAMILY_IPV4,
        2,
        attr(wire_neigh::attr::DST, &dst4[..2]),
        90,
    );
    assert_neigh_mutation_result(&request, Err(LinuxError::EINVAL));

    let request = build_neigh_mutation(
        RTM_DELNEIGH,
        wire_route::FAMILY_IPV6,
        0,
        delete_attrs6(),
        91,
    );
    assert_neigh_mutation_result(&request, Err(LinuxError::EAFNOSUPPORT));

    let request = build_neigh_mutation(
        RTM_DELNEIGH,
        wire_route::FAMILY_IPV4,
        0,
        delete_attrs4(),
        92,
    );
    assert_neigh_mutation_result(&request, Err(LinuxError::EINVAL));

    let request = build_neigh_mutation(RTM_DELNEIGH, wire_route::FAMILY_IPV4, 9999, Vec::new(), 93);
    assert_neigh_mutation_result(&request, Err(LinuxError::EINVAL));
}

#[def_test(serial)]
fn test_delneigh_ignores_non_destination_attributes() {
    init_test_state();
    let dev = NetDeviceId::new(2).unwrap();
    let dst = Ipv4Address::new(192, 168, 1, 94);
    let lladdr = [0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0xee];
    let add = neigh_mutation_request(
        RTM_NEWNEIGH,
        wire_neigh::STATE_PERMANENT,
        [
            attr(wire_neigh::attr::DST, &dst.octets()),
            attr(wire_neigh::attr::LLADDR, &lladdr),
        ]
        .concat(),
        &[],
        75,
    );
    assert_neigh_mutation_result(&add, Ok(()));

    // Linux `neigh_delete` only locates NDA_DST. It does not validate or even
    // decode NDA_LLADDR, so a malformed link address is irrelevant here.
    let delete = neigh_mutation_request(
        RTM_DELNEIGH,
        wire_neigh::STATE_PERMANENT,
        [
            attr(wire_neigh::attr::DST, &dst.octets()),
            attr(wire_neigh::attr::LLADDR, &[0xff]),
        ]
        .concat(),
        &[],
        76,
    );
    assert_neigh_mutation_result(&delete, Ok(()));
    assert!(SERVICE.neighbor_snapshot(dev, kernel_ipv4(dst)).is_none());
}

#[def_test(serial)]
fn test_neigh_mutations_reject_zero_ifindex_with_einval() {
    init_test_state();
    let dev = NetDeviceId::new(2).unwrap();
    let existing = Ipv4Address::new(192, 168, 1, 95);
    let add_existing = neigh_mutation_request(
        RTM_NEWNEIGH,
        wire_neigh::STATE_PERMANENT,
        [
            attr(wire_neigh::attr::DST, &existing.octets()),
            attr(wire_neigh::attr::LLADDR, &[0x02, 1, 2, 3, 4, 5]),
        ]
        .concat(),
        &[],
        77,
    );
    assert_neigh_mutation_result(&add_existing, Ok(()));

    // `neigh_add` leaves ifindex zero unresolved and later reports the missing
    // device as EINVAL, not ENODEV.
    let new_dst = Ipv4Address::new(192, 168, 1, 96);
    let mut payload = neigh_request_payload(
        wire_route::FAMILY_IPV4,
        0,
        wire_neigh::STATE_PERMANENT,
        0,
        0,
        None,
    );
    payload.extend(attr(wire_neigh::attr::DST, &new_dst.octets()));
    payload.extend(attr(wire_neigh::attr::LLADDR, &[0x02, 6, 7, 8, 9, 10]));
    let request = build_nlmsg(
        RTM_NEWNEIGH,
        78,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        payload,
    );
    assert_neigh_mutation_result(&request, Err(LinuxError::EINVAL));
    assert!(
        SERVICE
            .neighbor_snapshot(dev, kernel_ipv4(new_dst))
            .is_none()
    );

    // `neigh_delete` resolves only a nonzero ifindex and has the same EINVAL
    // outcome for zero, leaving the existing entry untouched.
    let mut payload = neigh_request_payload(
        wire_route::FAMILY_IPV4,
        0,
        wire_neigh::STATE_PERMANENT,
        0,
        0,
        None,
    );
    payload.extend(attr(wire_neigh::attr::DST, &existing.octets()));
    let request = build_nlmsg(RTM_DELNEIGH, 79, NLM_F_REQUEST | NLM_F_ACK, payload);
    assert_neigh_mutation_result(&request, Err(LinuxError::EINVAL));
    assert!(
        SERVICE
            .neighbor_snapshot(dev, kernel_ipv4(existing))
            .is_some()
    );
}

#[def_test(serial)]
fn test_delneigh_proxy_returns_enoent_and_keeps_neighbor() {
    init_test_state();
    let dev = NetDeviceId::new(2).unwrap();
    let dst = Ipv4Address::new(192, 168, 1, 97);
    let add = neigh_mutation_request(
        RTM_NEWNEIGH,
        wire_neigh::STATE_PERMANENT,
        [
            attr(wire_neigh::attr::DST, &dst.octets()),
            attr(wire_neigh::attr::LLADDR, &[0x02, 11, 12, 13, 14, 15]),
        ]
        .concat(),
        &[],
        80,
    );
    assert_neigh_mutation_result(&add, Ok(()));

    // There is no proxy table, so a proxy delete must not fall through to the
    // ordinary neighbor table. Linux returns ENOENT for a missing proxy entry.
    let request = build_nlmsg(
        RTM_DELNEIGH,
        81,
        NLM_F_REQUEST | NLM_F_ACK,
        neigh_request_payload(
            wire_route::FAMILY_IPV4,
            2,
            wire_neigh::STATE_PERMANENT,
            wire_neigh::NTF_PROXY,
            0,
            Some(dst.octets()),
        ),
    );
    assert_neigh_mutation_result(&request, Err(LinuxError::ENOENT));
    assert!(SERVICE.neighbor_snapshot(dev, kernel_ipv4(dst)).is_some());
}

#[def_test(serial)]
fn test_newneigh_rejects_unmodelled_extended_state() {
    init_test_state();
    let dev = NetDeviceId::new(2).unwrap();

    // Linux maps `NDA_FLAGS_EXT` into the internal NTF_MANAGED bit and
    // `NDA_PROTOCOL` into the stored neighbor protocol. Neither property is
    // represented here, so a nonzero value must fail instead of being ignored.
    let ext_dst = Ipv4Address::new(192, 168, 1, 98);
    let ext_request = neigh_mutation_request(
        RTM_NEWNEIGH,
        wire_neigh::STATE_PERMANENT,
        [
            attr(wire_neigh::attr::DST, &ext_dst.octets()),
            attr(wire_neigh::attr::LLADDR, &[0x02, 21, 22, 23, 24, 25]),
            attr(wire_neigh::attr::FLAGS_EXT, &1u32.to_ne_bytes()),
        ]
        .concat(),
        &[],
        82,
    );
    assert_neigh_mutation_result(&ext_request, Err(LinuxError::EOPNOTSUPP));
    assert!(
        SERVICE
            .neighbor_snapshot(dev, kernel_ipv4(ext_dst))
            .is_none()
    );

    let protocol_dst = Ipv4Address::new(192, 168, 1, 99);
    let protocol_request = neigh_mutation_request(
        RTM_NEWNEIGH,
        wire_neigh::STATE_PERMANENT,
        [
            attr(wire_neigh::attr::DST, &protocol_dst.octets()),
            attr(wire_neigh::attr::LLADDR, &[0x02, 31, 32, 33, 34, 35]),
            attr(wire_neigh::attr::PROTOCOL, &[1]),
        ]
        .concat(),
        &[],
        83,
    );
    assert_neigh_mutation_result(&protocol_request, Err(LinuxError::EOPNOTSUPP));
    assert!(
        SERVICE
            .neighbor_snapshot(dev, kernel_ipv4(protocol_dst))
            .is_none()
    );

    // A zero-valued extended flag and protocol are equivalent to omission and
    // remain accepted.
    let zero_dst = Ipv4Address::new(192, 168, 1, 100);
    let zero_request = neigh_mutation_request(
        RTM_NEWNEIGH,
        wire_neigh::STATE_PERMANENT,
        [
            attr(wire_neigh::attr::DST, &zero_dst.octets()),
            attr(wire_neigh::attr::LLADDR, &[0x02, 41, 42, 43, 44, 45]),
            attr(wire_neigh::attr::FLAGS_EXT, &0u32.to_ne_bytes()),
            attr(wire_neigh::attr::PROTOCOL, &[0]),
        ]
        .concat(),
        &[],
        84,
    );
    assert_neigh_mutation_result(&zero_request, Ok(()));
    assert!(
        SERVICE
            .neighbor_snapshot(dev, kernel_ipv4(zero_dst))
            .is_some()
    );
}

#[def_test(serial)]
fn test_neighbor_notifications_report_dynamic_state_and_unicast_type() {
    let dev = NetDeviceId::new(2).unwrap();
    let dst = crate::ip::IpAddress::Ipv4(crate::ip::Ipv4Address::new(192, 168, 1, 12));
    let state_reachable = NeighborState::Reachable {
        hardware_addr: [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
    };
    let state_permanent = NeighborState::Permanent {
        hardware_addr: [1, 2, 3, 4, 5, 6],
    };

    let cases = [
        (
            NetworkEvent::NeighborAdded {
                neighbor: NeighborUpdate {
                    dev,
                    dst,
                    state: NeighborState::Incomplete,
                },
            },
            RTM_NEWNEIGH,
            wire_neigh::STATE_INCOMPLETE,
            None,
        ),
        (
            NetworkEvent::NeighborAdded {
                neighbor: NeighborUpdate {
                    dev,
                    dst,
                    state: state_reachable,
                },
            },
            RTM_NEWNEIGH,
            wire_neigh::STATE_REACHABLE,
            Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]),
        ),
        (
            NetworkEvent::NeighborRemoved {
                neighbor: NeighborSnapshot {
                    dev,
                    dst,
                    state: state_permanent,
                },
            },
            RTM_DELNEIGH,
            wire_neigh::STATE_PERMANENT,
            Some([1, 2, 3, 4, 5, 6]),
        ),
    ];

    for (event, expected_type, expected_state, expected_lladdr) in cases {
        let notifications = notifications_for_events(vec![event]);
        let [notification] = notifications.as_slice() else {
            panic!("expected one neighbor notification");
        };
        assert_eq!(notification.group, RTNLGRP_NEIGH);
        let header = NlMsgHeader::read(&notification.data).unwrap();
        assert_eq!(
            (header.msg_type, header.seq, header.pid),
            (expected_type, 0, 0)
        );
        let msg = NdMsg::read(&notification.data[NLMSG_HDR_LEN..]).unwrap();
        let (family, ifindex, state, neigh_type) =
            (msg.family, msg.ifindex, msg.state, msg.neigh_type);
        assert_eq!(family, wire_route::FAMILY_IPV4);
        assert_eq!(ifindex, 2);
        assert_eq!(state, expected_state);
        // `neigh_fill_info` copies `inet_addr_type_dev_table()`, which is
        // RTN_UNICAST for ordinary IPv4 ARP entries.
        assert_eq!(neigh_type, RTN_UNICAST);
        let attrs = parse_attrs(&notification.data[NLMSG_HDR_LEN + NdMsg::SIZE..]).unwrap();
        assert_eq!(
            attrs
                .iter()
                .find(|attr| attr.kind == wire_neigh::attr::DST)
                .map(|attr| attr.payload),
            Some([192, 168, 1, 12].as_slice())
        );
        assert_eq!(
            attrs
                .iter()
                .find(|attr| attr.kind == wire_neigh::attr::LLADDR)
                .map(|attr| attr.payload),
            expected_lladdr.as_ref().map(|addr| addr.as_slice())
        );
    }
}

fn neigh_request_payload(
    family: u8,
    ifindex: i32,
    state: u16,
    flags: u8,
    neigh_type: u8,
    dst: Option<[u8; 4]>,
) -> Vec<u8> {
    let mut payload = Vec::new();
    NdMsg {
        family,
        pad1: 0,
        pad2: 0,
        ifindex,
        state,
        flags,
        neigh_type,
    }
    .write(&mut payload);
    if let Some(dst) = dst {
        payload.extend(attr(wire_neigh::attr::DST, &dst));
    }
    payload
}

fn kernel_ipv4(address: Ipv4Address) -> crate::ip::IpAddress {
    crate::ip::IpAddress::Ipv4(address.into())
}

fn neigh_mutation_request(
    msg_type: u16,
    state: u16,
    attrs: Vec<u8>,
    trailing: &[u8],
    seq: u32,
) -> Vec<u8> {
    let mut payload = neigh_request_payload(wire_route::FAMILY_IPV4, 2, state, 0, 0, None);
    payload.extend(attrs);
    payload.extend_from_slice(trailing);
    build_nlmsg(
        msg_type,
        seq,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        payload,
    )
}

fn build_neigh_mutation(
    msg_type: u16,
    family: u8,
    ifindex: i32,
    attrs: Vec<u8>,
    seq: u32,
) -> Vec<u8> {
    let state = if msg_type == RTM_NEWNEIGH {
        wire_neigh::STATE_PERMANENT
    } else {
        0
    };
    let mut payload = neigh_request_payload(family, ifindex, state, 0, 0, None);
    payload.extend(attrs);
    let create = if msg_type == RTM_NEWNEIGH {
        NLM_F_CREATE | NLM_F_EXCL
    } else {
        0
    };
    build_nlmsg(msg_type, seq, NLM_F_REQUEST | NLM_F_ACK | create, payload)
}

fn assert_neigh_mutation_result(request: &[u8], expected: Result<(), LinuxError>) {
    let packets = handle_rtnetlink_request(request, 0);
    let [packet] = packets.as_slice() else {
        panic!("expected one neighbor mutation response");
    };
    let msg_type = NlMsgHeader::read(&packet.data).unwrap().msg_type;
    assert_eq!(msg_type, NLMSG_ERROR);
    let expected_code = match expected {
        Ok(()) => 0,
        Err(error) => -error.into_raw(),
    };
    assert_eq!(
        read_i32_ne(&packet.data, NLMSG_HDR_LEN),
        Some(expected_code)
    );
}

fn get_neigh_request(ifindex: i32, dst: Option<[u8; 4]>, seq: u32) -> Vec<u8> {
    build_nlmsg(
        RTM_GETNEIGH,
        seq,
        NLM_F_REQUEST,
        neigh_request_payload(wire_route::FAMILY_IPV4, ifindex, 0, 0, 0, dst),
    )
}

fn dump_neigh_request(family: u8, ifindex: Option<i32>, master: Option<i32>) -> Vec<u8> {
    let mut payload = neigh_request_payload(family, 0, 0, 0, 0, None);
    if let Some(ifindex) = ifindex {
        payload.extend(attr(
            wire_neigh::attr::IFINDEX,
            &(ifindex as u32).to_ne_bytes(),
        ));
    }
    if let Some(master) = master {
        payload.extend(attr(
            wire_neigh::attr::MASTER,
            &(master as u32).to_ne_bytes(),
        ));
    }
    build_nlmsg(RTM_GETNEIGH, 88, NLM_F_REQUEST | NLM_F_DUMP, payload)
}

fn assert_get_neigh_error(request: &[u8], expected: LinuxError) {
    let packets = handle_rtnetlink_request(request, 0);
    let [packet] = packets.as_slice() else {
        panic!("expected one GETNEIGH error packet");
    };
    let msg_type = NlMsgHeader::read(&packet.data).unwrap().msg_type;
    assert_eq!(msg_type, NLMSG_ERROR);
    assert_eq!(
        read_i32_ne(&packet.data, NLMSG_HDR_LEN),
        Some(-expected.into_raw())
    );
}

#[def_test(serial)]
fn test_get_neigh_reports_request_sequence_and_port() {
    init_test_state();
    let dst = [192, 168, 1, 20];
    let lladdr = [2, 3, 4, 5, 6, 7];
    let dev = NetDeviceId::new(2).unwrap();
    add_neigh(
        NeighRequest {
            family: wire_route::FAMILY_IPV4,
            ifindex: 2,
            state: wire_neigh::STATE_PERMANENT,
            flags: 0,
            dst: IpAddress::Ipv4(Ipv4Address::new(dst[0], dst[1], dst[2], dst[3])),
            lladdr: Some(lladdr),
        },
        NLM_F_CREATE | NLM_F_EXCL,
    )
    .unwrap();

    let request = get_neigh_request(2, Some(dst), 91);
    let packets = handle_rtnetlink_request(&request, 4242);
    let [packet] = packets.as_slice() else {
        panic!("expected one GETNEIGH reply");
    };
    let header = NlMsgHeader::read(&packet.data).unwrap();
    let (msg_type, seq, pid, flags) = (header.msg_type, header.seq, header.pid, header.flags);
    assert_eq!((msg_type, seq, pid, flags), (RTM_NEWNEIGH, 91, 4242, 0));
    let msg = NdMsg::read(&packet.data[NLMSG_HDR_LEN..]).unwrap();
    assert_eq!(
        (msg.family, msg.ifindex, msg.state, msg.neigh_type),
        (
            wire_route::FAMILY_IPV4,
            2,
            wire_neigh::STATE_PERMANENT,
            RTN_UNICAST
        )
    );
    let attrs = parse_attrs(&packet.data[NLMSG_HDR_LEN + NdMsg::SIZE..]).unwrap();
    assert_eq!(
        attrs
            .iter()
            .find(|attr| attr.kind == wire_neigh::attr::DST)
            .map(|attr| attr.payload),
        Some(dst.as_slice())
    );
    assert_eq!(
        attrs
            .iter()
            .find(|attr| attr.kind == wire_neigh::attr::LLADDR)
            .map(|attr| attr.payload),
        Some(lladdr.as_slice())
    );

    // `neigh_fill_info` omits NDA_LLADDR for an entry without a valid
    // hardware address.
    SERVICE
        .apply_neighbor_update(
            NeighborUpdate {
                dev,
                dst: crate::ip::IpAddress::Ipv4(crate::ip::Ipv4Address::new(192, 168, 1, 21)),
                state: NeighborState::Incomplete,
            },
            crate::router::NeighborUpdatePolicy {
                can_create: true,
                exclusive: false,
                can_override: true,
            },
        )
        .unwrap();
    let packets =
        handle_rtnetlink_request(&get_neigh_request(2, Some([192, 168, 1, 21]), 92), 4242);
    let [packet] = packets.as_slice() else {
        panic!("expected one incomplete GETNEIGH reply");
    };
    let msg = NdMsg::read(&packet.data[NLMSG_HDR_LEN..]).unwrap();
    let state = msg.state;
    assert_eq!(state, wire_neigh::STATE_INCOMPLETE);
    let attrs = parse_attrs(&packet.data[NLMSG_HDR_LEN + NdMsg::SIZE..]).unwrap();
    assert!(
        !attrs
            .iter()
            .any(|attr| attr.kind == wire_neigh::attr::LLADDR)
    );
}

#[def_test(serial)]
fn test_get_neigh_validation_and_lookup_errors() {
    init_test_state();
    let dst = [192, 168, 1, 30];
    add_neigh(
        NeighRequest {
            family: wire_route::FAMILY_IPV4,
            ifindex: 2,
            state: wire_neigh::STATE_PERMANENT,
            flags: 0,
            dst: IpAddress::Ipv4(Ipv4Address::new(dst[0], dst[1], dst[2], dst[3])),
            lladdr: Some([2, 3, 4, 5, 6, 8]),
        },
        NLM_F_CREATE | NLM_F_EXCL,
    )
    .unwrap();

    let mut padding = neigh_request_payload(wire_route::FAMILY_IPV4, 2, 0, 0, 0, Some(dst));
    padding[1] = 1;
    assert_get_neigh_error(
        &build_nlmsg(RTM_GETNEIGH, 2, NLM_F_REQUEST, padding),
        LinuxError::EINVAL,
    );

    let unknown_flag = neigh_request_payload(wire_route::FAMILY_IPV4, 2, 0, 0x80, 0, Some(dst));
    assert_get_neigh_error(
        &build_nlmsg(RTM_GETNEIGH, 3, NLM_F_REQUEST, unknown_flag),
        LinuxError::EINVAL,
    );

    // The no-device header check precedes attribute parsing, so it wins even
    // when the family is unsupported or the attributes are invalid.
    let missing_device =
        neigh_request_payload(wire_route::FAMILY_IPV6, 0, 0, 0, 0, Some([192, 168, 1, 30]));
    assert_get_neigh_error(
        &build_nlmsg(RTM_GETNEIGH, 19, NLM_F_REQUEST, missing_device),
        LinuxError::EINVAL,
    );

    // Attribute validation precedes family lookup, matching
    // `neigh_valid_get_req`'s parse-before-family ordering.
    let mut unsupported = neigh_request_payload(wire_route::FAMILY_IPV6, 2, 0, 0, 0, None);
    unsupported.extend(attr(99, &[0; 4]));
    assert_get_neigh_error(
        &build_nlmsg(RTM_GETNEIGH, 4, NLM_F_REQUEST, unsupported),
        LinuxError::EINVAL,
    );

    let mut policy_invalid = neigh_request_payload(wire_route::FAMILY_IPV6, 2, 0, 0, 0, None);
    policy_invalid.extend(attr(wire_neigh::attr::CACHEINFO, &[0; 4]));
    assert_get_neigh_error(
        &build_nlmsg(RTM_GETNEIGH, 15, NLM_F_REQUEST, policy_invalid),
        LinuxError::EINVAL,
    );

    let mut unknown_attr = neigh_request_payload(wire_route::FAMILY_IPV4, 2, 0, 0, 0, Some(dst));
    unknown_attr.extend(attr(99, &[0; 4]));
    assert_get_neigh_error(
        &build_nlmsg(RTM_GETNEIGH, 5, NLM_F_REQUEST, unknown_attr),
        LinuxError::EINVAL,
    );

    let mut masked_unknown = neigh_request_payload(wire_route::FAMILY_IPV4, 2, 0, 0, 0, Some(dst));
    masked_unknown.extend(attr(0x8000 | (wire_neigh::attr::MAX + 1), &[0; 4]));
    assert_get_neigh_error(
        &build_nlmsg(RTM_GETNEIGH, 16, NLM_F_REQUEST, masked_unknown),
        LinuxError::EINVAL,
    );

    let mut trailing = neigh_request_payload(wire_route::FAMILY_IPV4, 2, 0, 0, 0, Some(dst));
    trailing.extend([0, 0]);
    assert_get_neigh_error(
        &build_nlmsg(RTM_GETNEIGH, 17, NLM_F_REQUEST, trailing),
        LinuxError::EINVAL,
    );

    // `NL_VALIDATE_MAXTYPE` rejects the unspecified attribute type too.
    let mut type_zero = neigh_request_payload(wire_route::FAMILY_IPV4, 2, 0, 0, 0, Some(dst));
    type_zero.extend(attr(0, &[]));
    assert_get_neigh_error(
        &build_nlmsg(RTM_GETNEIGH, 18, NLM_F_REQUEST, type_zero),
        LinuxError::EINVAL,
    );

    // `neigh_valid_get_req()` requires the destination before resolving the
    // interface, so a missing NDA_DST is EINVAL even for an unknown ifindex.
    assert_get_neigh_error(&get_neigh_request(99, None, 6), LinuxError::EINVAL);
    assert_get_neigh_error(&get_neigh_request(2, None, 7), LinuxError::EINVAL);

    // `neigh_get()` selects the family table before checking the address
    // length and the device, so an unsupported family wins over both.
    let unsupported_valid = neigh_request_payload(
        wire_route::FAMILY_IPV6,
        99,
        0,
        0,
        0,
        Some([192, 168, 1, 30]),
    );
    assert_get_neigh_error(
        &build_nlmsg(RTM_GETNEIGH, 20, NLM_F_REQUEST, unsupported_valid),
        LinuxError::EAFNOSUPPORT,
    );
    let mut unsupported_short = neigh_request_payload(wire_route::FAMILY_IPV6, 99, 0, 0, 0, None);
    unsupported_short.extend(attr(wire_neigh::attr::DST, &[192, 168, 1]));
    assert_get_neigh_error(
        &build_nlmsg(RTM_GETNEIGH, 21, NLM_F_REQUEST, unsupported_short),
        LinuxError::EAFNOSUPPORT,
    );

    // For IPv4 the family key length is checked before the device lookup.
    let mut short_dst = neigh_request_payload(wire_route::FAMILY_IPV4, 99, 0, 0, 0, None);
    short_dst.extend(attr(wire_neigh::attr::DST, &[192, 168, 1]));
    assert_get_neigh_error(
        &build_nlmsg(RTM_GETNEIGH, 22, NLM_F_REQUEST, short_dst),
        LinuxError::EINVAL,
    );

    let mut proxy = neigh_request_payload(
        wire_route::FAMILY_IPV4,
        2,
        0,
        wire_neigh::NTF_PROXY,
        0,
        None,
    );
    proxy.extend(attr(wire_neigh::attr::DST, &dst));
    assert_get_neigh_error(
        &build_nlmsg(RTM_GETNEIGH, 8, NLM_F_REQUEST, proxy),
        LinuxError::ENOENT,
    );

    assert_get_neigh_error(&get_neigh_request(0, Some(dst), 9), LinuxError::EINVAL);
    assert_get_neigh_error(
        &get_neigh_request(2, Some([192, 168, 1, 31]), 10),
        LinuxError::ENOENT,
    );
}

#[def_test(serial)]
fn test_neigh_dump_filters_and_unfiltered_replies() {
    init_test_state();
    let dev = NetDeviceId::new(2).unwrap();
    add_neigh(
        NeighRequest {
            family: wire_route::FAMILY_IPV4,
            ifindex: 2,
            state: wire_neigh::STATE_PERMANENT,
            flags: 0,
            dst: IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 40)),
            lladdr: Some([2, 3, 4, 5, 6, 9]),
        },
        NLM_F_CREATE | NLM_F_EXCL,
    )
    .unwrap();
    SERVICE
        .apply_neighbor_update(
            NeighborUpdate {
                dev,
                dst: crate::ip::IpAddress::Ipv4(crate::ip::Ipv4Address::new(192, 168, 1, 41)),
                state: NeighborState::Incomplete,
            },
            crate::router::NeighborUpdatePolicy {
                can_create: true,
                exclusive: false,
                can_override: true,
            },
        )
        .unwrap();

    let packets = handle_rtnetlink_request(&dump_neigh_request(0, None, None), 777);
    assert_eq!(packets.len(), 3);
    for packet in &packets[..2] {
        let header = NlMsgHeader::read(&packet.data).unwrap();
        let (msg_type, flags, seq, pid) = (header.msg_type, header.flags, header.seq, header.pid);
        assert_eq!(
            (msg_type, flags, seq, pid),
            (RTM_NEWNEIGH, NLM_F_MULTI, 88, 777)
        );
    }
    let msg_type = NlMsgHeader::read(&packets[2].data).unwrap().msg_type;
    assert_eq!(msg_type, NLMSG_DONE);
    assert_eq!(read_i32_ne(&packets[2].data, NLMSG_HDR_LEN), Some(0));

    // An unsupported family, proxy mode, or a positive master filter can
    // never match an entry in this stack; the dump still terminates cleanly.
    for request in [
        dump_neigh_request(wire_route::FAMILY_IPV6, None, None),
        dump_neigh_request(wire_route::FAMILY_IPV4, None, Some(3)),
    ] {
        let packets = handle_rtnetlink_request(&request, 777);
        let [done] = packets.as_slice() else {
            panic!("expected an empty filtered dump");
        };
        let msg_type = NlMsgHeader::read(&done.data).unwrap().msg_type;
        assert_eq!(msg_type, NLMSG_DONE);
    }

    let proxy = neigh_request_payload(
        wire_route::FAMILY_IPV4,
        0,
        0,
        wire_neigh::NTF_PROXY,
        0,
        None,
    );
    let request = build_nlmsg(RTM_GETNEIGH, 88, NLM_F_REQUEST | NLM_F_DUMP, proxy);
    let packets = handle_rtnetlink_request(&request, 777);
    let [done] = packets.as_slice() else {
        panic!("expected an empty proxy dump");
    };
    let msg_type = NlMsgHeader::read(&done.data).unwrap().msg_type;
    assert_eq!(msg_type, NLMSG_DONE);

    let packets = handle_rtnetlink_request(&dump_neigh_request(0, Some(2), None), 777);
    assert_eq!(packets.len(), 3);
    for packet in &packets[..2] {
        let flags = NlMsgHeader::read(&packet.data).unwrap().flags;
        assert_eq!(flags, NLM_F_MULTI | NLM_F_DUMP_FILTERED);
    }

    // `NDA_MASTER=-1` explicitly selects devices without a master, which is
    // every device in this stack.
    let packets = handle_rtnetlink_request(&dump_neigh_request(0, None, Some(-1)), 777);
    assert_eq!(packets.len(), 3);
    let flags = NlMsgHeader::read(&packets[0].data).unwrap().flags;
    assert_eq!(flags, NLM_F_MULTI | NLM_F_DUMP_FILTERED);

    let packets = handle_rtnetlink_request(&dump_neigh_request(0, Some(1), None), 777);
    let [done] = packets.as_slice() else {
        panic!("expected an empty interface-filtered dump");
    };
    let msg_type = NlMsgHeader::read(&done.data).unwrap().msg_type;
    assert_eq!(msg_type, NLMSG_DONE);

    // A complete `ndmsg` header selects the normal neighbor dump.
    let request = build_nlmsg(
        RTM_GETNEIGH,
        88,
        NLM_F_REQUEST | NLM_F_DUMP,
        neigh_request_payload(0, 0, 0, 0, 0, None),
    );
    let packets = handle_rtnetlink_request(&request, 777);
    assert_eq!(packets.len(), 3);

    // Liberal dump parsing ignores a malformed attribute list instead of
    // failing the request.
    let mut malformed = neigh_request_payload(0, 0, 0, 0, 0, None);
    malformed.extend([3, 0, 99, 0]);
    let request = build_nlmsg(RTM_GETNEIGH, 88, NLM_F_REQUEST | NLM_F_DUMP, malformed);
    let packets = handle_rtnetlink_request(&request, 777);
    assert_eq!(packets.len(), 3);
    let flags = NlMsgHeader::read(&packets[0].data).unwrap().flags;
    assert_eq!(flags, NLM_F_MULTI);

    // Structurally malformed trailing bytes preserve filters parsed before
    // the bad attribute.
    let mut filtered_malformed = neigh_request_payload(0, 0, 0, 0, 0, None);
    filtered_malformed.extend(attr(wire_neigh::attr::IFINDEX, &2u32.to_ne_bytes()));
    filtered_malformed.extend([3, 0, 99, 0]);
    let request = build_nlmsg(
        RTM_GETNEIGH,
        88,
        NLM_F_REQUEST | NLM_F_DUMP,
        filtered_malformed,
    );
    let packets = handle_rtnetlink_request(&request, 777);
    assert_eq!(packets.len(), 3);
    for packet in &packets[..2] {
        let flags = NlMsgHeader::read(&packet.data).unwrap().flags;
        assert_eq!(flags, NLM_F_MULTI | NLM_F_DUMP_FILTERED);
    }

    // A policy failure discards every parsed filter, matching Linux leaving
    // the caller's attribute table untouched on `-EINVAL`.
    let mut bad_policy = neigh_request_payload(0, 0, 0, 0, 0, None);
    bad_policy.extend(attr(wire_neigh::attr::IFINDEX, &2u32.to_ne_bytes()));
    bad_policy.extend(attr(wire_neigh::attr::MASTER, &[0, 0]));
    let request = build_nlmsg(RTM_GETNEIGH, 88, NLM_F_REQUEST | NLM_F_DUMP, bad_policy);
    let packets = handle_rtnetlink_request(&request, 777);
    assert_eq!(packets.len(), 3);
    let flags = NlMsgHeader::read(&packets[0].data).unwrap().flags;
    assert_eq!(flags, NLM_F_MULTI);

    // An explicit NDA_IFINDEX=0 does not set DUMP_FILTERED.
    let mut zero_ifindex = neigh_request_payload(0, 0, 0, 0, 0, None);
    zero_ifindex.extend(attr(wire_neigh::attr::IFINDEX, &0u32.to_ne_bytes()));
    let request = build_nlmsg(RTM_GETNEIGH, 88, NLM_F_REQUEST | NLM_F_DUMP, zero_ifindex);
    let packets = handle_rtnetlink_request(&request, 777);
    assert_eq!(packets.len(), 3);
    let flags = NlMsgHeader::read(&packets[0].data).unwrap().flags;
    assert_eq!(flags, NLM_F_MULTI);

    // `nda_policy[NDA_UNSPEC].strict_start_type = NDA_NH_ID` upgrades every
    // attribute from NDA_NH_ID onwards to NL_VALIDATE_STRICT.  A policy
    // failure must discard the valid IFINDEX parsed before it, while valid
    // strict-start attributes leave the filter intact.
    const NLA_F_NESTED: u16 = 0x8000;
    let dump_flags = |tail: &[u8]| {
        let mut payload = neigh_request_payload(0, 0, 0, 0, 0, None);
        payload.extend(attr(wire_neigh::attr::IFINDEX, &2u32.to_ne_bytes()));
        payload.extend_from_slice(tail);
        let request = build_nlmsg(RTM_GETNEIGH, 88, NLM_F_REQUEST | NLM_F_DUMP, payload);
        let packets = handle_rtnetlink_request(&request, 777);
        assert_eq!(packets.len(), 3);
        NlMsgHeader::read(&packets[0].data).unwrap().flags
    };

    for valid in [
        attr(wire_neigh::attr::NH_ID, &7u32.to_ne_bytes()),
        attr(NLA_F_NESTED | wire_neigh::attr::FDB_EXT_ATTRS, &[]),
        attr(
            NLA_F_NESTED | wire_neigh::attr::FDB_EXT_ATTRS,
            &[4, 0, 0, 0],
        ),
        attr(wire_neigh::attr::FLAGS_EXT, &0u32.to_ne_bytes()),
        attr(wire_neigh::attr::FLAGS_EXT, &1u32.to_ne_bytes()),
    ] {
        assert_eq!(
            dump_flags(&valid),
            NLM_F_MULTI | NLM_F_DUMP_FILTERED,
            "valid strict-start attribute unexpectedly reset the dump filter"
        );
    }

    for invalid in [
        // NDA_NH_ID is NLA_U32 and must be exactly four bytes.
        attr(wire_neigh::attr::NH_ID, &[0u8; 8]),
        attr(NLA_F_NESTED | wire_neigh::attr::NH_ID, &7u32.to_ne_bytes()),
        // NLA_NESTED requires NLA_F_NESTED and a payload of 0 or >= 4 bytes.
        attr(wire_neigh::attr::FDB_EXT_ATTRS, &[]),
        attr(wire_neigh::attr::FDB_EXT_ATTRS, &[4, 0, 0, 0]),
        attr(NLA_F_NESTED | wire_neigh::attr::FDB_EXT_ATTRS, &[4, 0, 0]),
        // NDA_FLAGS_EXT is an exact-width masked NLA_U32.
        attr(wire_neigh::attr::FLAGS_EXT, &[0u8; 8]),
        attr(
            NLA_F_NESTED | wire_neigh::attr::FLAGS_EXT,
            &1u32.to_ne_bytes(),
        ),
        attr(wire_neigh::attr::FLAGS_EXT, &2u32.to_ne_bytes()),
        // NDA_NDM_STATE_MASK and NDA_NDM_FLAGS_MASK have NLA_UNSPEC policy.
        attr(
            wire_neigh::attr::NDM_STATE_MASK,
            &wire_neigh::STATE_REACHABLE.to_ne_bytes(),
        ),
        attr(wire_neigh::attr::NDM_FLAGS_MASK, &1u32.to_ne_bytes()),
    ] {
        assert_eq!(
            dump_flags(&invalid),
            NLM_F_MULTI,
            "strict-start policy failure retained the dump filter"
        );
    }
}

#[def_test(serial)]
fn test_dump_routes_returns_done_message() {
    init_test_state();
    let header = nl_header(RTM_GETROUTE, NLM_F_REQUEST, 7);
    let packets = handle_rtnetlink_request(
        &build_nlmsg(
            RTM_GETROUTE,
            7,
            NLM_F_REQUEST | NLM_F_DUMP,
            vec![wire_route::FAMILY_IPV4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        ),
        0,
    );

    assert!(packets.len() >= 2);
    let last = packets.last().unwrap();
    let seq = header.seq;
    assert_eq!(read_u16_ne(&last.data, 4), Some(NLMSG_DONE));
    assert_eq!(read_i32_ne(&last.data, NLMSG_HDR_LEN), Some(0));
    assert_eq!(seq, 7);

    // Every route dump entry is a `fib_dump_info` message and therefore
    // carries both the compact table byte and the full-width RTA_TABLE.
    for packet in &packets[..packets.len() - 1] {
        assert_eq!(read_u16_ne(&packet.data, 4), Some(RTM_NEWROUTE));
        let info = RtMsg::read(&packet.data[NLMSG_HDR_LEN..]).unwrap();
        let attrs = parse_attrs(&packet.data[NLMSG_HDR_LEN + RtMsg::SIZE..]).unwrap();
        let table = attrs
            .iter()
            .find(|attr| attr.kind == wire_route::attr::TABLE)
            .expect("route dump RTA_TABLE");
        assert_eq!(table.payload, u32::from(info.table).to_ne_bytes());
    }
}

#[def_test(serial)]
fn test_route_dump_with_prefix_flag_returns_no_routes() {
    init_test_state();
    let mut payload = vec![wire_route::FAMILY_IPV4, 0, 0, 0, 0, 0, 0, 0];
    payload.extend_from_slice(&wire_route::FLAG_PREFIX.to_ne_bytes());
    let packets = handle_rtnetlink_request(
        &build_nlmsg(RTM_GETROUTE, 8, NLM_F_REQUEST | NLM_F_DUMP, payload),
        0,
    );

    assert_eq!(packets.len(), 1);
    let done = &packets[0];
    assert_eq!(read_u16_ne(&done.data, 4), Some(NLMSG_DONE));
    assert_eq!(read_i32_ne(&done.data, NLMSG_HDR_LEN), Some(0));
}

#[def_test(serial)]
fn test_rtnetlink_dispatches_with_a_one_byte_rtgenmsg() {
    init_test_state();

    // A request with no payload is shorter than `struct rtgenmsg` and is
    // ignored, so `NLM_F_ACK` still produces a successful acknowledgement.
    let empty = handle_rtnetlink_request(
        &build_nlmsg(RTM_GETLINK, 1610, NLM_F_REQUEST | NLM_F_ACK, Vec::new()),
        0,
    );
    assert_eq!(empty.len(), 1);
    assert_eq!(read_u16_ne(&empty[0].data, 4), Some(NLMSG_ERROR));
    assert_eq!(read_i32_ne(&empty[0].data, NLMSG_HDR_LEN), Some(0));

    // One byte supplies the complete family field.  Dispatch reaches the
    // GETLINK handler, which rejects the missing `ifinfomsg`.
    for payload_len in [1, 3] {
        let packets = handle_rtnetlink_request(
            &build_nlmsg(
                RTM_GETLINK,
                1611 + payload_len as u32,
                NLM_F_REQUEST | NLM_F_ACK,
                vec![wire_route::FAMILY_IPV4; payload_len],
            ),
            0,
        );
        assert_eq!(packets.len(), 1);
        assert_eq!(read_u16_ne(&packets[0].data, 4), Some(NLMSG_ERROR));
        assert_eq!(
            read_i32_ne(&packets[0].data, NLMSG_HDR_LEN),
            Some(-LinuxError::EINVAL.into_raw())
        );
    }
}

#[def_test(serial)]
fn test_ipv6_family_dumps_do_not_leak_ipv4_records() {
    init_test_state();

    let mut addr_payload = Vec::new();
    IfAddrMsg {
        family: wire_route::FAMILY_IPV6,
        prefix_len: 0,
        flags: 0,
        scope: 0,
        index: 0,
    }
    .write(&mut addr_payload);
    let addr_packets = handle_rtnetlink_request(
        &build_nlmsg(RTM_GETADDR, 1601, NLM_F_REQUEST | NLM_F_DUMP, addr_payload),
        0,
    );
    assert_eq!(addr_packets.len(), 1);
    assert_eq!(read_u16_ne(&addr_packets[0].data, 4), Some(NLMSG_DONE));

    let mut route_payload = Vec::new();
    RtMsg {
        family: wire_route::FAMILY_IPV6,
        dst_len: 0,
        src_len: 0,
        tos: 0,
        table: 0,
        protocol: 0,
        scope: 0,
        route_type: 0,
        flags: 0,
    }
    .write(&mut route_payload);
    let route_packets = handle_rtnetlink_request(
        &build_nlmsg(
            RTM_GETROUTE,
            1602,
            NLM_F_REQUEST | NLM_F_DUMP,
            route_payload,
        ),
        0,
    );
    assert_eq!(route_packets.len(), 1);
    assert_eq!(read_u16_ne(&route_packets[0].data, 4), Some(NLMSG_DONE));
}

fn getroute_request(
    seq: u32,
    netlink_flags: u16,
    route_flags: u32,
    tos: u8,
    attrs: &[Vec<u8>],
) -> Vec<u8> {
    let mut payload = Vec::new();
    RtMsg {
        family: wire_route::FAMILY_IPV4,
        dst_len: 0,
        src_len: 0,
        tos,
        table: 0,
        protocol: 0,
        scope: 0,
        route_type: 0,
        flags: route_flags,
    }
    .write(&mut payload);
    for item in attrs {
        payload.extend_from_slice(item);
    }
    build_nlmsg(RTM_GETROUTE, seq, netlink_flags, payload)
}

#[derive(Debug)]
struct GetRouteReplyHeader {
    pid: u32,
    seq: u32,
}

#[derive(Debug)]
struct GetRouteReplyInfo {
    family: u8,
    dst_len: u8,
    src_len: u8,
    tos: u8,
    table: u8,
    protocol: u8,
    scope: u8,
    route_type: u8,
    flags: u32,
}

fn getroute_reply_parts(
    packet: &NetlinkPacket,
) -> (GetRouteReplyHeader, GetRouteReplyInfo, Vec<(u16, Vec<u8>)>) {
    let header = NlMsgHeader::read(&packet.data).unwrap();
    let msg_type = header.msg_type;
    assert_eq!(msg_type, RTM_NEWROUTE);
    let end = header.len as usize;
    let info = RtMsg::read(&packet.data[NLMSG_HDR_LEN..end]).unwrap();
    let info = GetRouteReplyInfo {
        family: info.family,
        dst_len: info.dst_len,
        src_len: info.src_len,
        tos: info.tos,
        table: info.table,
        protocol: info.protocol,
        scope: info.scope,
        route_type: info.route_type,
        flags: info.flags,
    };
    let attrs = parse_attrs(&packet.data[NLMSG_HDR_LEN + RtMsg::SIZE..end])
        .unwrap()
        .into_iter()
        .map(|item| (item.kind, item.payload.to_vec()))
        .collect();
    (
        GetRouteReplyHeader {
            pid: header.pid,
            seq: header.seq,
        },
        info,
        attrs,
    )
}

fn getroute_errno(error: LinuxError) -> i32 {
    -error.into_raw()
}

fn getroute_reply_attr(attrs: &[(u16, Vec<u8>)], kind: u16) -> Option<Vec<u8>> {
    attrs
        .iter()
        .find(|(item_kind, _)| *item_kind == kind)
        .map(|(_, value)| value.clone())
}

fn getroute_reply_u32(attrs: &[(u16, Vec<u8>)], kind: u16) -> Option<u32> {
    getroute_reply_attr(attrs, kind).map(|value| u32::from_ne_bytes(value[..4].try_into().unwrap()))
}

fn getroute_error(request: &[u8]) -> i32 {
    let packets = handle_rtnetlink_request(request, 0);
    assert_eq!(packets.len(), 1);
    assert_eq!(read_u16_ne(&packets[0].data, 4), Some(NLMSG_ERROR));
    read_i32_ne(&packets[0].data, NLMSG_HDR_LEN).unwrap()
}

#[def_test(serial)]
fn test_route_dump_ignores_family_table_and_cloned_flag() {
    init_test_state();
    let mut payload = Vec::new();
    RtMsg {
        // PF_AX25 has no rtnetlink route handler, so the dispatcher falls
        // back to the PF_UNSPEC dump-all callback.
        family: 3,
        dst_len: 0,
        src_len: 0,
        tos: 0,
        table: 42,
        protocol: 0,
        scope: 0,
        route_type: 0,
        flags: 0x200,
    }
    .write(&mut payload);

    let packets = handle_rtnetlink_request(
        &build_nlmsg(RTM_GETROUTE, 9, NLM_F_REQUEST | NLM_F_DUMP, payload),
        0,
    );
    let routes = packets
        .iter()
        .filter(|packet| read_u16_ne(&packet.data, 4) == Some(RTM_NEWROUTE))
        .count();

    // The fallback dump-all callback reaches the IPv4 FIB dump.  Its
    // non-strict form ignores the requested table, and `RTM_F_CLONED` is only
    // interpreted by the strict filter, so neither suppresses the route walk.
    assert_eq!(routes, SERVICE.route_snapshot().len());
    assert!(routes > 0);
}

#[def_test(serial)]
fn test_getroute_unicast_reply_matches_linux_lookup_and_ack() {
    init_test_state();
    let mut request = getroute_request(
        1201,
        NLM_F_REQUEST | NLM_F_ACK,
        0,
        0xff,
        &[
            attr(wire_route::attr::DST, &[8, 8, 8, 8]),
            attr(wire_route::attr::MARK, &7u32.to_ne_bytes()),
            attr(wire_route::attr::UID, &1234u32.to_ne_bytes()),
        ],
    );
    let packets = handle_rtnetlink_request(&request, 7001);
    assert_eq!(packets.len(), 2);

    let (header, info, attrs) = getroute_reply_parts(&packets[0]);
    assert_eq!(header.pid, 7001);
    assert_eq!(header.seq, 1201);
    assert_eq!(info.family, wire_route::FAMILY_IPV4);
    assert_eq!(info.dst_len, 32);
    assert_eq!(info.src_len, 0);
    // `ip_route_output_key_hash_rcu` applies `IPTOS_RT_MASK` (0x1c), not the
    // whole TOS byte, before storing the flow TOS.
    assert_eq!(info.tos, 0x1c);
    assert_eq!(info.table, wire_route::TABLE_MAIN);
    assert_eq!(info.protocol, 0);
    assert_eq!(info.scope, wire_route::SCOPE_UNIVERSE);
    assert_eq!(info.route_type, wire_route::TYPE_UNICAST);
    assert_eq!(info.flags, wire_route::FLAG_CLONED);

    assert_eq!(
        attrs.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(),
        vec![
            wire_route::attr::TABLE,
            wire_route::attr::DST,
            wire_route::attr::OIF,
            wire_route::attr::PREFSRC,
            wire_route::attr::GATEWAY,
            wire_route::attr::MARK,
            wire_route::attr::UID,
            wire_route::attr::CACHEINFO,
        ]
    );
    assert_eq!(
        getroute_reply_attr(&attrs, wire_route::attr::PREFSRC),
        Some(vec![192, 168, 1, 2])
    );
    assert_eq!(
        getroute_reply_u32(&attrs, wire_route::attr::TABLE),
        Some(u32::from(wire_route::TABLE_MAIN))
    );
    assert_eq!(
        getroute_reply_attr(&attrs, wire_route::attr::DST),
        Some([8, 8, 8, 8].to_vec())
    );
    assert_eq!(getroute_reply_u32(&attrs, wire_route::attr::OIF), Some(2));
    assert_eq!(
        getroute_reply_attr(&attrs, wire_route::attr::GATEWAY),
        Some([192, 168, 1, 1].to_vec())
    );
    assert_eq!(getroute_reply_u32(&attrs, wire_route::attr::MARK), Some(7));
    assert_eq!(
        getroute_reply_u32(&attrs, wire_route::attr::UID),
        Some(1234)
    );
    assert_eq!(
        getroute_reply_attr(&attrs, wire_route::attr::CACHEINFO),
        Some(vec![0; 32])
    );

    assert_eq!(read_i32_ne(&packets[1].data, NLMSG_HDR_LEN), Some(0));
    assert_eq!(read_u16_ne(&packets[1].data, 4), Some(NLMSG_ERROR));

    // No ACK is synthesized unless `NLM_F_ACK` was requested.
    request = getroute_request(
        1202,
        NLM_F_REQUEST,
        0,
        0,
        &[attr(wire_route::attr::DST, &[8, 8, 8, 8])],
    );
    assert_eq!(handle_rtnetlink_request(&request, 7001).len(), 1);

    // An explicit source is echoed in `RTA_SRC` with `src_len`, and a selected
    // source equal to the request source suppresses `RTA_PREFSRC`.
    request = getroute_request(
        1203,
        NLM_F_REQUEST,
        wire_route::FLAG_NOTIFY,
        0,
        &[
            attr(wire_route::attr::DST, &[8, 8, 8, 8]),
            attr(wire_route::attr::SRC, &[192, 168, 1, 2]),
        ],
    );
    let packets = handle_rtnetlink_request(&request, 0);
    assert_eq!(packets.len(), 1);
    let (_, info, attrs) = getroute_reply_parts(&packets[0]);
    assert_eq!(info.src_len, 32);
    assert_eq!(
        info.flags,
        wire_route::FLAG_CLONED | wire_route::FLAG_NOTIFY
    );
    assert_eq!(
        getroute_reply_attr(&attrs, wire_route::attr::SRC),
        Some([192, 168, 1, 2].to_vec())
    );
    assert_eq!(getroute_reply_attr(&attrs, wire_route::attr::PREFSRC), None);

    // An explicitly zero source is treated as absent for reply source
    // attributes, but the selected source is still reported in `RTA_PREFSRC`.
    request = getroute_request(
        1204,
        NLM_F_REQUEST,
        0,
        0,
        &[
            attr(wire_route::attr::DST, &[8, 8, 8, 8]),
            attr(wire_route::attr::SRC, &0u32.to_ne_bytes()),
        ],
    );
    let packets = handle_rtnetlink_request(&request, 0);
    let (_, info, attrs) = getroute_reply_parts(&packets[0]);
    assert_eq!(info.src_len, 0);
    assert_eq!(getroute_reply_attr(&attrs, wire_route::attr::SRC), None);
    assert_eq!(
        getroute_reply_attr(&attrs, wire_route::attr::PREFSRC),
        Some([192, 168, 1, 2].to_vec())
    );
    assert_eq!(getroute_reply_u32(&attrs, wire_route::attr::UID), Some(0));
}

#[def_test(serial)]
fn test_getroute_special_destinations_report_linux_route_flags() {
    init_test_state();

    let query = |seq, dst: [u8; 4], oif: u32, flags: u32| {
        let mut attrs = vec![attr(wire_route::attr::DST, &dst)];
        if oif != 0 {
            attrs.push(attr(wire_route::attr::OIF, &oif.to_ne_bytes()));
        }
        let request = getroute_request(seq, NLM_F_REQUEST, flags, 0, &attrs);
        let packets = handle_rtnetlink_request(&request, 0);
        assert_eq!(packets.len(), 1);
        getroute_reply_parts(&packets[0])
    };

    let (_, info, attrs) = query(1301, [255, 255, 255, 255], 0, 0);
    assert_eq!(info.route_type, wire_route::TYPE_BROADCAST);
    assert_eq!(
        info.flags,
        wire_route::FLAG_CLONED | wire_route::RTCF_BROADCAST | wire_route::RTCF_LOCAL
    );
    assert_eq!(info.table, wire_route::TABLE_MAIN);
    assert_eq!(getroute_reply_u32(&attrs, wire_route::attr::OIF), Some(2));
    assert_eq!(
        getroute_reply_attr(&attrs, wire_route::attr::PREFSRC),
        Some([192, 168, 1, 2].to_vec())
    );
    // `__mkroute_output` clears `fi` for every broadcast route, so the default
    // gateway is not part of the resulting route.
    assert_eq!(getroute_reply_attr(&attrs, wire_route::attr::GATEWAY), None);

    // The OIF shortcut for limited broadcast skips the FIB and leaves
    // `res.table` empty; `RTM_F_LOOKUP_TABLE` therefore reports table 0.
    let (_, info, attrs) = query(1302, [255, 255, 255, 255], 2, wire_route::FLAG_LOOKUP_TABLE);
    assert_eq!(info.route_type, wire_route::TYPE_BROADCAST);
    assert_eq!(info.table, 0);
    assert_eq!(getroute_reply_u32(&attrs, wire_route::attr::TABLE), Some(0));
    assert_eq!(getroute_reply_attr(&attrs, wire_route::attr::GATEWAY), None);

    // IPv4 multicast through the default route keeps `fib_entry`, but
    // `__mkroute_output` deliberately suppresses a gateway when the matched
    // prefix is shorter than /4.  Without an IP multicast membership database
    // the non-member branch also clears `RTCF_LOCAL`.
    let (_, info, attrs) = query(1303, [239, 1, 1, 1], 0, 0);
    assert_eq!(info.route_type, wire_route::TYPE_MULTICAST);
    assert_eq!(
        info.flags,
        wire_route::FLAG_CLONED | wire_route::RTCF_MULTICAST
    );
    assert_eq!(info.table, wire_route::TABLE_MAIN);
    assert_eq!(getroute_reply_u32(&attrs, wire_route::attr::OIF), Some(2));
    assert_eq!(
        getroute_reply_attr(&attrs, wire_route::attr::PREFSRC),
        Some([192, 168, 1, 2].to_vec())
    );
    assert_eq!(getroute_reply_attr(&attrs, wire_route::attr::GATEWAY), None);

    let (_, info, attrs) = query(1304, [127, 0, 0, 1], 0, wire_route::FLAG_LOOKUP_TABLE);
    assert_eq!(info.route_type, wire_route::TYPE_LOCAL);
    assert_eq!(info.flags, wire_route::FLAG_CLONED | wire_route::RTCF_LOCAL);
    assert_eq!(info.table, wire_route::TABLE_LOCAL);
    assert_eq!(getroute_reply_u32(&attrs, wire_route::attr::OIF), Some(1));

    // An explicit OIF constrains the FIB lookup through the route's owner
    // device, not the loopback output rewrite stored on a local route.  The
    // local address is owned by eth0, so the lookup succeeds and the reply
    // still reports the loopback output device.
    let (_, info, attrs) = query(1305, [192, 168, 1, 2], 2, wire_route::FLAG_LOOKUP_TABLE);
    assert_eq!(info.route_type, wire_route::TYPE_LOCAL);
    assert_eq!(info.flags, wire_route::FLAG_CLONED | wire_route::RTCF_LOCAL);
    assert_eq!(info.table, wire_route::TABLE_LOCAL);
    assert_eq!(getroute_reply_u32(&attrs, wire_route::attr::OIF), Some(1));
    assert_eq!(
        getroute_reply_attr(&attrs, wire_route::attr::PREFSRC),
        Some([192, 168, 1, 2].to_vec())
    );

    // A zero destination is rewritten to the selected local source and sent
    // over loopback, while the reply still reports the original `RTA_DST`.
    let (_, info, attrs) = query(1306, [0, 0, 0, 0], 0, wire_route::FLAG_LOOKUP_TABLE);
    assert_eq!(info.route_type, wire_route::TYPE_LOCAL);
    assert_eq!(info.flags, wire_route::FLAG_CLONED | wire_route::RTCF_LOCAL);
    assert_eq!(info.table, 0);
    assert_eq!(getroute_reply_u32(&attrs, wire_route::attr::OIF), Some(1));
    assert_eq!(
        getroute_reply_attr(&attrs, wire_route::attr::PREFSRC),
        Some([127, 0, 0, 1].to_vec())
    );
    assert_eq!(
        getroute_reply_attr(&attrs, wire_route::attr::DST),
        Some(vec![0, 0, 0, 0])
    );

    // A subnet broadcast is a FIB broadcast route.  It is not the limited
    // broadcast checked by the source/OIF shortcut, and its table is the local
    // table selected by the route's notification identity.
    let (_, info, _) = query(1307, [192, 168, 1, 255], 0, wire_route::FLAG_LOOKUP_TABLE);
    assert_eq!(info.route_type, wire_route::TYPE_BROADCAST);
    assert_eq!(info.table, wire_route::TABLE_LOCAL);
    assert_eq!(
        info.flags,
        wire_route::FLAG_CLONED | wire_route::RTCF_BROADCAST | wire_route::RTCF_LOCAL
    );
}

#[def_test(serial)]
fn test_getroute_fib_match_uses_matched_rule_and_final_type() {
    init_test_state();

    let query = |seq, dst: [u8; 4], flags: u32| {
        let request = getroute_request(
            seq,
            NLM_F_REQUEST,
            flags | wire_route::FLAG_FIB_MATCH,
            0,
            &[attr(wire_route::attr::DST, &dst)],
        );
        let packets = handle_rtnetlink_request(&request, 0);
        assert_eq!(packets.len(), 1);
        getroute_reply_parts(&packets[0])
    };

    let (_, info, attrs) = query(1401, [192, 168, 1, 9], 0);
    assert_eq!(info.dst_len, 24);
    assert_eq!(info.src_len, 0);
    assert_eq!(info.table, wire_route::TABLE_MAIN);
    assert_eq!(info.protocol, wire_route::PROTOCOL_KERNEL);
    assert_eq!(info.scope, wire_route::SCOPE_LINK);
    assert_eq!(info.route_type, wire_route::TYPE_UNICAST);
    assert_eq!(info.flags, 0);
    assert_eq!(
        getroute_reply_attr(&attrs, wire_route::attr::DST),
        Some([192, 168, 1, 0].to_vec())
    );
    assert_eq!(
        getroute_reply_attr(&attrs, wire_route::attr::PREFSRC),
        Some([192, 168, 1, 2].to_vec())
    );
    assert_eq!(getroute_reply_u32(&attrs, wire_route::attr::OIF), Some(2));
    assert_eq!(
        getroute_reply_attr(&attrs, wire_route::attr::CACHEINFO),
        None
    );

    let (_, info, attrs) = query(1402, [127, 0, 0, 1], wire_route::FLAG_LOOKUP_TABLE);
    assert_eq!(info.dst_len, 32);
    assert_eq!(info.table, wire_route::TABLE_LOCAL);
    assert_eq!(info.protocol, wire_route::PROTOCOL_KERNEL);
    assert_eq!(info.scope, wire_route::SCOPE_HOST);
    assert_eq!(info.route_type, wire_route::TYPE_LOCAL);
    assert_eq!(info.flags, 0);
    assert_eq!(
        getroute_reply_attr(&attrs, wire_route::attr::DST),
        Some([127, 0, 0, 1].to_vec())
    );
    assert_eq!(getroute_reply_u32(&attrs, wire_route::attr::OIF), Some(1));

    // FIB_MATCH reports the matched default route (including its gateway), but
    // the response type is the final output route type, not the FIB type.
    let (_, info, attrs) = query(1403, [255, 255, 255, 255], 0);
    assert_eq!(info.dst_len, 0);
    assert_eq!(info.table, wire_route::TABLE_MAIN);
    assert_eq!(info.protocol, wire_route::PROTOCOL_BOOT);
    assert_eq!(info.scope, wire_route::SCOPE_UNIVERSE);
    assert_eq!(info.route_type, wire_route::TYPE_BROADCAST);
    assert_eq!(info.flags, 0);
    assert_eq!(getroute_reply_attr(&attrs, wire_route::attr::DST), None);
    assert_eq!(
        getroute_reply_attr(&attrs, wire_route::attr::GATEWAY),
        Some([192, 168, 1, 1].to_vec())
    );

    let (_, info, attrs) = query(1404, [239, 1, 1, 1], 0);
    assert_eq!(info.dst_len, 0);
    assert_eq!(info.table, wire_route::TABLE_MAIN);
    assert_eq!(info.protocol, wire_route::PROTOCOL_BOOT);
    assert_eq!(info.route_type, wire_route::TYPE_MULTICAST);
    assert_eq!(
        getroute_reply_attr(&attrs, wire_route::attr::GATEWAY),
        Some([192, 168, 1, 1].to_vec())
    );

    // The source/OIF multicast shortcut constructs a route with no FIB entry;
    // Linux's FIB_MATCH path cannot fabricate one and returns EHOSTUNREACH.
    let request = getroute_request(
        1405,
        NLM_F_REQUEST,
        wire_route::FLAG_FIB_MATCH,
        0,
        &[
            attr(wire_route::attr::DST, &[239, 1, 1, 1]),
            attr(wire_route::attr::SRC, &[192, 168, 1, 2]),
        ],
    );
    assert_eq!(
        getroute_error(&request),
        getroute_errno(LinuxError::EHOSTUNREACH)
    );
}

#[def_test(serial)]
fn test_getroute_validation_order_and_liberal_attributes() {
    init_test_state();

    // The IPv4 doer requires a complete `rtmsg`, not just `rtgenmsg`.
    let short = build_nlmsg(RTM_GETROUTE, 1501, NLM_F_REQUEST, vec![2; 11]);
    assert_eq!(getroute_error(&short), getroute_errno(LinuxError::EINVAL));

    // The rtnetlink dispatcher selects the family callback from the first
    // payload byte.  AF_UNSPEC and unknown families have no registered `doit`.
    for family in [0, 10] {
        let mut payload = Vec::new();
        RtMsg {
            family,
            dst_len: 0,
            src_len: 0,
            tos: 0,
            table: 0,
            protocol: 0,
            scope: 0,
            route_type: 0,
            flags: 0,
        }
        .write(&mut payload);
        let request = build_nlmsg(RTM_GETROUTE, 1502, NLM_F_REQUEST, payload);
        assert_eq!(
            getroute_error(&request),
            getroute_errno(LinuxError::EOPNOTSUPP)
        );
    }

    // NLA_U32 route attributes accept payloads longer than four bytes.
    let mut valid_attrs = getroute_request(
        1503,
        NLM_F_REQUEST,
        0,
        0,
        &[attr(wire_route::attr::DST, &[8, 8, 8, 8, 0, 0, 0, 0])],
    );
    let packets = handle_rtnetlink_request(&valid_attrs, 0);
    assert_eq!(packets.len(), 1);
    assert_eq!(
        getroute_reply_parts(&packets[0]).1.route_type,
        wire_route::TYPE_UNICAST
    );

    // Trailing partial attributes terminate non-strict parsing without
    // invalidating attributes already parsed.
    let mut payload = Vec::new();
    RtMsg {
        family: wire_route::FAMILY_IPV4,
        dst_len: 0,
        src_len: 0,
        tos: 0,
        table: 0,
        protocol: 0,
        scope: 0,
        route_type: 0,
        flags: 0,
    }
    .write(&mut payload);
    payload.extend(attr(wire_route::attr::DST, &[8, 8, 8, 8]));
    payload.extend_from_slice(&[1, 0]);
    valid_attrs = build_nlmsg(RTM_GETROUTE, 1504, NLM_F_REQUEST, payload);
    assert_eq!(handle_rtnetlink_request(&valid_attrs, 0).len(), 1);

    let invalid_attrs = |tail: Vec<u8>| {
        let mut payload = Vec::new();
        RtMsg {
            family: wire_route::FAMILY_IPV4,
            dst_len: 0,
            src_len: 0,
            tos: 0,
            table: 0,
            protocol: 0,
            scope: 0,
            route_type: 0,
            flags: 0,
        }
        .write(&mut payload);
        payload.extend(tail);
        build_nlmsg(RTM_GETROUTE, 1505, NLM_F_REQUEST, payload)
    };
    for request in [
        invalid_attrs(attr(wire_route::attr::DST, &[8, 8])),
        invalid_attrs(attr(wire_route::attr::IP_PROTO, &[])),
        invalid_attrs(attr(wire_route::attr::SPORT, &[0])),
        invalid_attrs(attr(wire_route::attr::NH_ID, &[0; 8])),
        invalid_attrs(attr(wire_route::attr::METRICS, &[0; 3])),
        invalid_attrs(attr(wire_route::attr::MULTIPATH, &[0; 7])),
    ] {
        assert_eq!(
            getroute_error(&request),
            getroute_errno(LinuxError::EINVAL),
            "malformed route attribute was accepted"
        );
    }

    // `RTA_IP_PROTO` is validated before the IIF device lookup; an unsupported
    // protocol therefore wins over the missing interface error.
    let igmp_with_missing_iif = getroute_request(
        1506,
        NLM_F_REQUEST,
        0,
        0,
        &[
            attr(wire_route::attr::IP_PROTO, &[2]),
            attr(wire_route::attr::IIF, &99u32.to_ne_bytes()),
        ],
    );
    assert_eq!(
        getroute_error(&igmp_with_missing_iif),
        getroute_errno(LinuxError::EOPNOTSUPP)
    );

    let tcp_with_missing_iif = getroute_request(
        1507,
        NLM_F_REQUEST,
        0,
        0,
        &[
            attr(wire_route::attr::IP_PROTO, &[wire_route::IPPROTO_TCP]),
            attr(wire_route::attr::IIF, &99u32.to_ne_bytes()),
        ],
    );
    assert_eq!(
        getroute_error(&tcp_with_missing_iif),
        getroute_errno(LinuxError::ENODEV)
    );

    // The project has no input-route implementation; an existing interface is
    // rejected explicitly rather than being silently ignored.
    let tcp_with_iif = getroute_request(
        1508,
        NLM_F_REQUEST,
        0,
        0,
        &[
            attr(wire_route::attr::DST, &[192, 168, 1, 2]),
            attr(wire_route::attr::IP_PROTO, &[wire_route::IPPROTO_TCP]),
            attr(wire_route::attr::IIF, &1u32.to_ne_bytes()),
        ],
    );
    assert_eq!(
        getroute_error(&tcp_with_iif),
        getroute_errno(LinuxError::EOPNOTSUPP)
    );

    let missing_oif = getroute_request(
        1509,
        NLM_F_REQUEST,
        0,
        0,
        &[
            attr(wire_route::attr::DST, &[8, 8, 8, 8]),
            attr(wire_route::attr::OIF, &99u32.to_ne_bytes()),
        ],
    );
    assert_eq!(
        getroute_error(&missing_oif),
        getroute_errno(LinuxError::ENODEV)
    );

    // Source validation precedes FIB lookup: multicast and limited broadcast
    // sources are invalid, while an unknown unicast source is unreachable.
    // `0.0.0.1` is not `ipv4_is_zeronet()` (which is exactly `0.0.0.0`), so it
    // follows the unknown-source path instead of the EINVAL path.
    for source in [[224, 0, 0, 1], [255, 255, 255, 255]] {
        let request = getroute_request(
            1510,
            NLM_F_REQUEST,
            0,
            0,
            &[
                attr(wire_route::attr::DST, &[8, 8, 8, 8]),
                attr(wire_route::attr::SRC, &source),
            ],
        );
        assert_eq!(
            getroute_error(&request),
            getroute_errno(LinuxError::EINVAL),
            "invalid source {source:?} was accepted"
        );
    }
    for source in [[10, 1, 1, 1], [0, 0, 0, 1]] {
        let unknown_source = getroute_request(
            1511,
            NLM_F_REQUEST,
            0,
            0,
            &[
                attr(wire_route::attr::DST, &[8, 8, 8, 8]),
                attr(wire_route::attr::SRC, &source),
            ],
        );
        assert_eq!(
            getroute_error(&unknown_source),
            getroute_errno(LinuxError::ENETUNREACH),
            "non-local source {source:?} was not unreachable"
        );
    }

    // Error replies are produced even without `NLM_F_ACK`; requesting an ACK
    // does not duplicate the error response.
    let no_ack = build_nlmsg(RTM_GETROUTE, 1512, NLM_F_REQUEST, vec![2; 11]);
    assert_eq!(getroute_error(&no_ack), getroute_errno(LinuxError::EINVAL));
    let with_ack = build_nlmsg(RTM_GETROUTE, 1513, NLM_F_REQUEST | NLM_F_ACK, vec![2; 11]);
    assert_eq!(
        getroute_error(&with_ack),
        getroute_errno(LinuxError::EINVAL)
    );
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
    let packets = handle_rtnetlink_request(&request, 0);

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

    let packets = handle_rtnetlink_request(&request, 0);

    assert_eq!(packets.len(), 1);
    assert_eq!(read_i32_ne(&packets[0].data, NLMSG_HDR_LEN), Some(0));
    assert!(
        SERVICE
            .ipv4_addr_entries()
            .iter()
            .any(|(_, entry)| entry.addr.address() == address.into())
    );
}

#[def_test(serial)]
fn test_newaddr_no_prefix_route_suppresses_only_connected_route() {
    init_test_state();
    let address = Ipv4Address::new(192, 168, 9, 79);
    let flags = u32::from(wire_addr::FLAG_PERMANENT) | wire_addr::FLAG_NOPREFIXROUTE;
    let request = build_ipv4_addr_mutation_with_attrs(
        address,
        24,
        15,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        &[(wire_addr::attr::FLAGS, &flags.to_ne_bytes())],
    );

    let packets = handle_rtnetlink_request(&request, 0);
    assert_eq!(read_i32_ne(&packets[0].data, NLMSG_HDR_LEN), Some(0));

    assert!(SERVICE.route_snapshot().iter().any(|route| {
        route.filter.prefix_len() == 32
            && route.filter.address()
                == crate::ip::IpAddress::Ipv4(crate::ip::Ipv4Address::from_octets(address.octets()))
    }));
    assert!(SERVICE.route_snapshot().iter().all(|route| {
        route.filter.prefix_len() != 24
            || route.filter.address()
                != crate::ip::IpAddress::Ipv4(crate::ip::Ipv4Address::from_octets([192, 168, 9, 0]))
    }));
    // `IFA_F_NOPREFIXROUTE` does not suppress the computed network broadcast,
    // whose preferred source remains the address that produced it.
    let broadcast = SERVICE
        .route_snapshot()
        .into_iter()
        .find(|route| {
            route.filter.prefix_len() == 32
                && route.filter.address()
                    == crate::ip::IpAddress::Ipv4(crate::ip::Ipv4Address::from_octets([
                        192, 168, 9, 255,
                    ]))
                && route.route_type == RTN_BROADCAST
        })
        .expect("computed broadcast route");
    assert_eq!(
        broadcast.preferred_source(),
        Some(crate::ip::IpAddress::Ipv4(
            crate::ip::Ipv4Address::from_octets(address.octets())
        ))
    );

    let packets = handle_rtnetlink_request(
        &build_nlmsg(
            RTM_GETADDR,
            16,
            NLM_F_REQUEST | NLM_F_DUMP,
            addr_dump_payload(),
        ),
        0,
    );
    let packet = packets
        .iter()
        .find(|packet| {
            let payload = &packet.data[NLMSG_HDR_LEN..];
            IfAddrMsg::read(payload).is_ok_and(|info| {
                info.index == 2
                    && parse_attrs(&payload[IfAddrMsg::SIZE..]).is_ok_and(|attrs| {
                        attrs.iter().any(|attr| {
                            attr.kind == wire_addr::attr::LOCAL && attr.payload == address.octets()
                        })
                    })
            })
        })
        .expect("new address dump");
    let payload = &packet.data[NLMSG_HDR_LEN..];
    let info = IfAddrMsg::read(payload).unwrap();
    assert_eq!(info.flags, wire_addr::FLAG_PERMANENT);
    let attrs = parse_attrs(&payload[IfAddrMsg::SIZE..]).unwrap();
    let flags = attrs
        .iter()
        .find(|attr| attr.kind == wire_addr::attr::FLAGS)
        .expect("IFA_FLAGS");
    assert_eq!(
        u32::from_ne_bytes(flags.payload.try_into().unwrap()),
        u32::from(wire_addr::FLAG_PERMANENT) | wire_addr::FLAG_NOPREFIXROUTE
    );
}

#[def_test(serial)]
fn test_newaddr_rejects_unsupported_ipv4_address_flags() {
    init_test_state();
    let mut request = build_ipv4_addr_mutation(
        Ipv4Address::new(192, 168, 1, 80),
        17,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE,
    );
    let mut flags_attr = attr(
        wire_addr::attr::FLAGS,
        // TENTATIVE is not modeled by the permanent-address subset.  The
        // derived SECONDARY bit is intentionally accepted and normalized.
        &u32::from(wire_addr::FLAG_PERMANENT | 0x2).to_ne_bytes(),
    );
    request.append(&mut flags_attr);
    let len = request.len() as u32;
    request[..4].copy_from_slice(&len.to_ne_bytes());

    let packets = handle_rtnetlink_request(&request, 0);
    assert_eq!(
        read_i32_ne(&packets[0].data, NLMSG_HDR_LEN),
        Some(-(LinuxError::EOPNOTSUPP.into_raw()))
    );
}

#[def_test(serial)]
fn test_newaddr_round_trips_ipv4_broadcast_and_reports_permanent_flags() {
    init_test_state();
    let address = Ipv4Address::new(192, 168, 2, 78);
    let broadcast = [192, 168, 1, 255];
    let mut request = build_ipv4_addr_mutation(
        address,
        13,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
    );
    let mut broadcast_attr = attr(wire_addr::attr::BROADCAST, &broadcast);
    request.append(&mut broadcast_attr);
    let len = request.len() as u32;
    request[..4].copy_from_slice(&len.to_ne_bytes());

    let packets = handle_rtnetlink_request(&request, 0);
    assert_eq!(read_i32_ne(&packets[0].data, NLMSG_HDR_LEN), Some(0));

    let packets = handle_rtnetlink_request(
        &build_nlmsg(
            RTM_GETADDR,
            14,
            NLM_F_REQUEST | NLM_F_DUMP,
            addr_dump_payload(),
        ),
        0,
    );
    let packet = packets
        .iter()
        .find(|packet| {
            let payload = &packet.data[NLMSG_HDR_LEN..];
            IfAddrMsg::read(payload).is_ok_and(|info| {
                let (index, prefix_len) = (info.index, info.prefix_len);
                index == 2
                    && prefix_len == 24
                    && parse_attrs(&payload[IfAddrMsg::SIZE..]).is_ok_and(|attrs| {
                        attrs.iter().any(|attr| {
                            attr.kind == wire_addr::attr::LOCAL && attr.payload == address.octets()
                        })
                    })
            })
        })
        .expect("new address dump");
    let payload = &packet.data[NLMSG_HDR_LEN..];
    let info = IfAddrMsg::read(payload).unwrap();
    assert_eq!(info.flags, wire_addr::FLAG_PERMANENT);
    let attrs = parse_attrs(&payload[IfAddrMsg::SIZE..]).unwrap();
    let broadcast_attr = attrs
        .iter()
        .find(|attr| attr.kind == wire_addr::attr::BROADCAST)
        .expect("IFA_BROADCAST");
    assert_eq!(broadcast_attr.payload, broadcast);
    let flags = attrs
        .iter()
        .find(|attr| attr.kind == wire_addr::attr::FLAGS)
        .expect("IFA_FLAGS");
    assert_eq!(
        u32::from_ne_bytes(flags.payload.try_into().unwrap()),
        u32::from(wire_addr::FLAG_PERMANENT)
    );
}

#[def_test(serial)]
fn test_publish_kobject_uevent_to_group_subscriber() {
    let socket = NetlinkSocket::new(NETLINK_KOBJECT_UEVENT, NetlinkSocketType::Raw);
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
    let socket = NetlinkSocket::new(NETLINK_KOBJECT_UEVENT, NetlinkSocketType::Raw);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 0, groups: 1 }))
        .unwrap();

    let next_seqnum = UEVENT_SEQNUM.load(Ordering::Relaxed) + 1;
    let seqnum_suffix_len = format!("SEQNUM={next_seqnum}\0").len();
    let payload = vec![0u8; NETLINK_RX_QUEUE_LIMIT - seqnum_suffix_len + 1];
    publish_kobject_uevent(1, &payload);
    publish_kobject_uevent(1, b"overflow");

    let mut queue = socket.inner.rx_queue.lock();
    let packet = queue.pop_front().unwrap();
    assert!(packet.data.starts_with(&payload));
    assert!(queue.pop_front().is_none());
}

#[def_test(serial)]
fn test_publish_kobject_uevent_appends_monotonic_seqnum() {
    let socket = NetlinkSocket::new(NETLINK_KOBJECT_UEVENT, NetlinkSocketType::Raw);
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
fn test_kobject_uevent_passcred_reports_kernel_credentials() {
    let socket = NetlinkSocket::new(NETLINK_KOBJECT_UEVENT, NetlinkSocketType::Raw);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 0, groups: 1 }))
        .unwrap();
    socket
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();
    publish_kobject_uevent(1, b"change@/devices/test\0");

    let mut data = [0u8; 128];
    let mut ancillary = Vec::new();
    socket
        .recv(
            Cursor::new(data.as_mut_slice()),
            RecvOptions {
                ancillary: Some(&mut ancillary),
                ..RecvOptions::default()
            },
        )
        .unwrap();

    let credentials = ancillary
        .pop()
        .unwrap()
        .downcast::<KernelAncillaryData>()
        .unwrap();
    let KernelAncillaryData::Credentials(credentials) = credentials.as_ref() else {
        panic!("expected SCM_CREDENTIALS ancillary data");
    };
    assert!(credentials.pid.is_none());
    assert_eq!(credentials.uid, 0);
    assert_eq!(credentials.gid, 0);
}

#[def_test(serial)]
fn test_route_netlink_passcred_reports_kernel_credentials() {
    let socket = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    socket
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();
    socket.inner.rx_queue.lock().push_back(NetlinkPacket {
        from: NetlinkAddr { pid: 0, groups: 0 },
        data: vec![1, 2, 3, 4],
        credentials: None,
    });

    let mut data = [0u8; 4];
    let mut ancillary = Vec::new();
    socket
        .recv(
            Cursor::new(data.as_mut_slice()),
            RecvOptions {
                ancillary: Some(&mut ancillary),
                ..RecvOptions::default()
            },
        )
        .unwrap();

    // A kernel-origin rtnetlink skb has zeroed control-block credentials;
    // SO_PASSCRED still reports them rather than omitting SCM_CREDENTIALS.
    let credentials = ancillary
        .pop()
        .unwrap()
        .downcast::<KernelAncillaryData>()
        .unwrap();
    let KernelAncillaryData::Credentials(credentials) = credentials.as_ref() else {
        panic!("expected SCM_CREDENTIALS ancillary data");
    };
    assert!(credentials.pid.is_none());
    assert_eq!(credentials.uid, 0);
    assert_eq!(credentials.gid, 0);
}

#[def_test(serial)]
fn test_route_netlink_generated_reply_and_ack_use_kernel_zero_credentials() {
    init_test_state();
    let socket = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 231,
            groups: 0,
        }))
        .unwrap();
    socket
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();

    let mut payload = Vec::new();
    IfInfoMsg {
        family: 0,
        pad: 0,
        link_type: 0,
        index: 0,
        flags: 0,
        change: 0,
    }
    .write(&mut payload);
    payload.extend(attr(wire_link::attr::IFNAME, b"eth0\0"));
    let request = build_nlmsg(RTM_GETLINK, 81, NLM_F_REQUEST | NLM_F_ACK, payload);
    assert_eq!(
        socket.send_with_cred(
            Cursor::new(request.as_slice()),
            SendOptions::default(),
            &Cred::root(),
        ),
        Ok(request.len())
    );

    for expected_type in [RTM_NEWLINK, NLMSG_ERROR] {
        let mut data = [0u8; 2048];
        let mut ancillary = Vec::new();
        socket
            .recv(
                Cursor::new(data.as_mut_slice()),
                RecvOptions {
                    ancillary: Some(&mut ancillary),
                    ..RecvOptions::default()
                },
            )
            .unwrap();
        assert_eq!(
            read_u16_ne(&data, 4),
            Some(expected_type),
            "generated response order"
        );
        let credentials = ancillary
            .pop()
            .expect("SO_PASSCRED response credentials")
            .downcast::<KernelAncillaryData>()
            .unwrap();
        let KernelAncillaryData::Credentials(credentials) = credentials.as_ref() else {
            panic!("expected SCM_CREDENTIALS ancillary data");
        };
        // Generated replies and ACKs are kernel-origin skbs. Linux does not
        // populate their control-block credentials, so SO_PASSCRED observes
        // the zeroed PID/UID/GID rather than the requester's identity.
        assert!(credentials.pid.is_none());
        assert_eq!(credentials.uid, 0);
        assert_eq!(credentials.gid, 0);
    }
}

#[def_test(serial)]
fn test_usersock_passcred_reports_claimed_identity_on_truncated_receive() {
    let receiver = NetlinkSocket::new(NETLINK_USERSOCK, NetlinkSocketType::Raw);
    receiver
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 218,
            groups: 0,
        }))
        .unwrap();
    receiver
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();
    let sender = NetlinkSocket::new(NETLINK_USERSOCK, NetlinkSocketType::Raw);
    sender
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 219,
            groups: 0,
        }))
        .unwrap();
    let message = build_nlmsg(
        NLMSG_MIN_TYPE,
        79,
        NLM_F_REQUEST,
        vec![0x11, 0x22, 0x33, 0x44],
    );
    let expected = crate::SenderCredentials {
        pid: Some(kidentity::PidHandle::fixed_root(1)),
        uid: 1234,
        gid: 5678,
    };
    assert_eq!(
        sender.send_with_cred(
            Cursor::new(message.as_slice()),
            SendOptions {
                to: Some(SocketAddrEx::Netlink(NetlinkAddr {
                    pid: 218,
                    groups: 0,
                })),
                sender_credentials: Some(expected.clone()),
                ..SendOptions::default()
            },
            &Cred::root(),
        ),
        Ok(message.len())
    );

    let mut data = [0u8; 2];
    let mut ancillary = Vec::new();
    let mut flags = RecvFlags::empty();
    assert_eq!(
        receiver.recv(
            Cursor::new(data.as_mut_slice()),
            RecvOptions {
                ancillary: Some(&mut ancillary),
                out_flags: Some(&mut flags),
                ..RecvOptions::default()
            },
        ),
        Ok(2)
    );
    assert!(flags.contains(RecvFlags::TRUNCATE));
    // Credentials come from the skb control block, so a short copy still
    // reports the sender even though the payload is truncated.
    let credentials = ancillary
        .pop()
        .unwrap()
        .downcast::<KernelAncillaryData>()
        .unwrap();
    let KernelAncillaryData::Credentials(credentials) = credentials.as_ref() else {
        panic!("expected SCM_CREDENTIALS ancillary data");
    };
    assert_eq!(
        credentials.pid.as_ref().map(|pid| pid.root_nr()),
        expected.pid.as_ref().map(|pid| pid.root_nr())
    );
    assert_eq!(credentials.uid, 1234);
    assert_eq!(credentials.gid, 5678);
}

#[def_test(serial, user)]
fn test_netlink_multicast_reports_the_sender_identity_per_datagram() {
    init_test_state();
    let subscriber = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    subscriber
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 214,
            groups: group_bit(RTNLGRP_LINK),
        }))
        .unwrap();
    subscriber
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();
    let sender = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    sender
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 215,
            groups: 0,
        }))
        .unwrap();

    let mut expected =
        crate::unix::current_sender_credentials().expect("a user thread has a sender identity");
    expected.uid = 1234;
    expected.gid = 5678;
    let message = build_nlmsg(
        RTM_GETLINK,
        76,
        NLM_F_REQUEST | NLM_F_DUMP,
        link_dump_payload(&[]),
    );
    assert_eq!(
        sender.send_with_cred(
            Cursor::new(message.as_slice()),
            SendOptions {
                to: Some(SocketAddrEx::Netlink(NetlinkAddr {
                    pid: 0,
                    groups: group_bit(RTNLGRP_LINK),
                })),
                sender_credentials: Some(expected.clone()),
                ..SendOptions::default()
            },
            &Cred::root(),
        ),
        Ok(message.len())
    );

    let mut data = vec![0u8; message.len()];
    let mut ancillary = Vec::new();
    assert_eq!(
        subscriber.recv(
            Cursor::new(data.as_mut_slice()),
            RecvOptions {
                ancillary: Some(&mut ancillary),
                ..RecvOptions::default()
            },
        ),
        Ok(message.len())
    );
    assert_eq!(data, message);
    let credentials = ancillary
        .pop()
        .unwrap()
        .downcast::<KernelAncillaryData>()
        .unwrap();
    let KernelAncillaryData::Credentials(credentials) = credentials.as_ref() else {
        panic!("expected SCM_CREDENTIALS ancillary data");
    };
    // The multicast skb carries the datagram sender captured at the socket
    // boundary, not the identity of the receiver or the kernel callback.
    assert_eq!(
        credentials.pid.as_ref().map(|pid| pid.root_nr()),
        expected.pid.as_ref().map(|pid| pid.root_nr())
    );
    assert_eq!(credentials.uid, 1234);
    assert_eq!(credentials.gid, 5678);
}

#[def_test(serial)]
fn test_netlink_sender_identity_does_not_authorize_route_mutation() {
    init_test_state();
    let socket = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 216,
            groups: 0,
        }))
        .unwrap();
    let address = Ipv4Address::new(10, 9, 8, 7);
    let mutation = build_ipv4_addr_mutation(
        address,
        77,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
    );

    // A claimed root control-block identity must not authorize the operation.
    // Linux checks the capability of the sending socket/current task, which
    // here is explicitly unprivileged.
    assert_eq!(
        socket.send_with_cred(
            Cursor::new(mutation.as_slice()),
            SendOptions {
                sender_credentials: Some(crate::SenderCredentials {
                    pid: Some(kidentity::PidHandle::fixed_root(1)),
                    uid: 0,
                    gid: 0,
                }),
                ..SendOptions::default()
            },
            &Cred::new(1000, 1000),
        ),
        Ok(mutation.len())
    );
    let error = socket.inner.rx_queue.lock().pop_front().unwrap();
    assert_eq!(
        read_i32_ne(&error.data, NLMSG_HDR_LEN),
        Some(-(LinuxError::EPERM.into_raw()))
    );
    assert!(!SERVICE.ipv4_addr_entries().iter().any(|(dev, entry)| {
        entry.addr.address() == address.into() && *dev == NetDeviceId::new(2).unwrap()
    }));
}

#[def_test(serial)]
fn test_netlink_kernel_command_requires_opener_and_sender_privilege() {
    init_test_state();
    let socket = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 221,
            groups: 0,
        }))
        .unwrap();

    let send = |address: Ipv4Address, options: SendOptions, cred: &Cred| {
        let mutation = build_ipv4_addr_mutation(
            address,
            81,
            NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        );
        let request_len = mutation.len();
        let result = socket.send_with_cred(Cursor::new(mutation.as_slice()), options, cred);
        let response = socket.inner.rx_queue.lock().pop_front().unwrap();
        (
            result,
            request_len,
            read_i32_ne(&response.data, NLMSG_HDR_LEN).unwrap(),
        )
    };

    // Addressless delivery has no explicit destination skb flag, so Linux
    // requires the capability in both the opener and the current sender.
    let (result, request_len, error) = send(
        Ipv4Address::new(10, 9, 8, 7),
        SendOptions {
            opener_cred: Some(Cred::new(1000, 1000)),
            ..SendOptions::default()
        },
        &Cred::root(),
    );
    assert_eq!(result, Ok(request_len));
    assert_eq!(error, -(LinuxError::EPERM.into_raw()));

    let (result, request_len, error) = send(
        Ipv4Address::new(10, 9, 8, 8),
        SendOptions {
            opener_cred: Some(Cred::root()),
            ..SendOptions::default()
        },
        &Cred::new(1000, 1000),
    );
    assert_eq!(result, Ok(request_len));
    assert_eq!(error, -(LinuxError::EPERM.into_raw()));

    // An explicit sockaddr, even one naming kernel PID 0, sets the destination
    // skb flag and therefore only the current sender's capability is tested.
    let (result, request_len, error) = send(
        Ipv4Address::new(10, 9, 8, 9),
        SendOptions {
            to: Some(SocketAddrEx::Netlink(NetlinkAddr { pid: 0, groups: 0 })),
            opener_cred: Some(Cred::new(1000, 1000)),
            ..SendOptions::default()
        },
        &Cred::root(),
    );
    assert_eq!(result, Ok(request_len));
    assert_eq!(error, 0);
}

#[def_test(serial)]
fn test_route_mutation_authorization_uses_capability_sets() {
    init_test_state();
    let socket = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 222,
            groups: 0,
        }))
        .unwrap();

    let send = |address: Ipv4Address, credential: &Cred| {
        let mutation = build_ipv4_addr_mutation(
            address,
            82,
            NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        );
        let request_len = mutation.len();
        let result = socket.send_with_cred(
            Cursor::new(mutation.as_slice()),
            SendOptions {
                opener_cred: Some(credential.clone()),
                ..SendOptions::default()
            },
            credential,
        );
        let response = socket.inner.rx_queue.lock().pop_front().unwrap();
        (
            result,
            request_len,
            read_i32_ne(&response.data, NLMSG_HDR_LEN).unwrap(),
        )
    };

    // Linux capability checks use the effective set, not the effective UID. A
    // non-root process granted CAP_NET_ADMIN may mutate routes.
    let net_admin = credential_with_capabilities(1000, capability(CAP_NET_ADMIN));
    let (result, request_len, error) = send(Ipv4Address::new(10, 9, 8, 10), &net_admin);
    assert_eq!(result, Ok(request_len));
    assert_eq!(error, 0);

    // Effective UID 0 with an empty effective capability set has no authority.
    let root_without_caps = credential_with_capabilities(0, CapSet::EMPTY);
    let (result, request_len, error) = send(Ipv4Address::new(10, 9, 8, 11), &root_without_caps);
    assert_eq!(result, Ok(request_len));
    assert_eq!(error, -(LinuxError::EPERM.into_raw()));

    // CAP_SYS_ADMIN does not substitute for the rtnetlink capability.
    let sys_admin = credential_with_capabilities(1000, capability(CAP_SYS_ADMIN));
    let (result, request_len, error) = send(Ipv4Address::new(10, 9, 8, 12), &sys_admin);
    assert_eq!(result, Ok(request_len));
    assert_eq!(error, -(LinuxError::EPERM.into_raw()));
}

#[def_test(serial)]
fn test_claimed_credentials_do_not_authorize_privileged_route_send() {
    let sender = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    sender
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 220,
            groups: 0,
        }))
        .unwrap();
    let message = build_nlmsg(
        RTM_GETLINK,
        80,
        NLM_F_REQUEST | NLM_F_DUMP,
        link_dump_payload(&[]),
    );

    // Authorization follows the capability of the sending task, never the
    // claimed SCM_CREDENTIALS identity carried by the skb.
    assert_eq!(
        sender.send_with_cred(
            Cursor::new(message.as_slice()),
            SendOptions {
                to: Some(SocketAddrEx::Netlink(NetlinkAddr {
                    pid: 0,
                    groups: group_bit(RTNLGRP_IPV4_MROUTE_R),
                })),
                sender_credentials: Some(crate::SenderCredentials {
                    pid: Some(kidentity::PidHandle::fixed_root(1)),
                    uid: 0,
                    gid: 0,
                }),
                ..SendOptions::default()
            },
            &Cred::new(1000, 1000),
        ),
        Err(LinuxError::EPERM.into())
    );
}

#[def_test(serial, user)]
fn test_kobject_uevent_forward_reports_origin_sender_identity() {
    let subscriber = NetlinkSocket::new(NETLINK_KOBJECT_UEVENT, NetlinkSocketType::Raw);
    subscriber
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 0, groups: 1 }))
        .unwrap();
    subscriber
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();
    let sender = NetlinkSocket::new(NETLINK_KOBJECT_UEVENT, NetlinkSocketType::Raw);
    sender
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 217,
            groups: 0,
        }))
        .unwrap();

    let mut expected =
        crate::unix::current_sender_credentials().expect("a user thread has a sender identity");
    expected.uid = 4321;
    expected.gid = 8765;
    let body = b"add@/devices/credential-test\0ACTION=add\0";
    let message = build_nlmsg(NLMSG_MIN_TYPE, 78, NLM_F_REQUEST, body.to_vec());
    assert_eq!(
        sender.send_with_cred(
            Cursor::new(message.as_slice()),
            SendOptions {
                sender_credentials: Some(expected.clone()),
                ..SendOptions::default()
            },
            &Cred::root(),
        ),
        Ok(message.len())
    );

    let mut data = [0u8; 256];
    let mut ancillary = Vec::new();
    let length = subscriber
        .recv(
            Cursor::new(data.as_mut_slice()),
            RecvOptions {
                ancillary: Some(&mut ancillary),
                ..RecvOptions::default()
            },
        )
        .unwrap();
    assert!(data[..length].starts_with(body));
    let credentials = ancillary
        .pop()
        .unwrap()
        .downcast::<KernelAncillaryData>()
        .unwrap();
    let KernelAncillaryData::Credentials(credentials) = credentials.as_ref() else {
        panic!("expected SCM_CREDENTIALS ancillary data");
    };
    // Userspace-originated uevents are forwarded with the originating skb's
    // credentials; only the kernel-originated publisher substitutes root/0.
    assert_eq!(
        credentials.pid.as_ref().map(|pid| pid.root_nr()),
        expected.pid.as_ref().map(|pid| pid.root_nr())
    );
    assert_eq!(credentials.uid, 4321);
    assert_eq!(credentials.gid, 8765);
}

#[def_test(serial)]
fn test_recv_reports_datagram_length_and_truncation() {
    let socket = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    socket.inner.rx_queue.lock().push_back(NetlinkPacket {
        from: NetlinkAddr { pid: 1, groups: 0 },
        data: vec![1, 2, 3, 4],
        credentials: None,
    });

    let mut probe = [];
    let mut probe_flags = RecvFlags::empty();
    let result = socket.recv(
        Cursor::new(probe.as_mut_slice()),
        RecvOptions {
            flags: RecvFlags::PEEK | RecvFlags::TRUNCATE,
            out_flags: Some(&mut probe_flags),
            ..RecvOptions::default()
        },
    );
    assert_eq!(result, Ok(4));
    assert!(probe_flags.contains(RecvFlags::TRUNCATE));

    let mut buf = [0u8; 2];
    let mut recv_flags = RecvFlags::empty();
    let result = socket.recv(
        Cursor::new(buf.as_mut_slice()),
        RecvOptions {
            out_flags: Some(&mut recv_flags),
            ..RecvOptions::default()
        },
    );
    assert_eq!(result, Ok(2));
    assert_eq!(buf, [1, 2]);
    assert!(recv_flags.contains(RecvFlags::TRUNCATE));

    let mut queue = socket.inner.rx_queue.lock();
    assert!(queue.pop_front().is_none());
}

#[def_test]
fn route_encap_attribute_numbers_match_linux() {
    // `RTA_ENCAP_TYPE` and `RTA_ENCAP` are adjacent in Linux's `rtattr_type_t`;
    // using the wrong number would silently validate the other attribute.
    assert_eq!(wire_route::attr::ENCAP_TYPE, 21);
    assert_eq!(wire_route::attr::ENCAP, 22);
}

#[def_test(serial)]
fn test_route_dump_ignores_nonzero_short_trailer() {
    init_test_state();
    let socket = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 42, groups: 0 }))
        .unwrap();

    let mut payload = vec![wire_route::FAMILY_IPV4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    push_attr(
        &mut payload,
        wire_route::attr::TABLE,
        &(wire_route::TABLE_MAIN as u32).to_ne_bytes(),
    );
    let mut request = build_nlmsg(RTM_GETROUTE, 43, NLM_F_REQUEST | NLM_F_DUMP, payload);
    // Fewer than NLMSG_HDR_LEN trailing bytes that are not zero padding.
    request.extend_from_slice(&[0xaa; 7]);

    assert_eq!(
        socket.send(Cursor::new(request.as_slice()), SendOptions::default()),
        Ok(request.len())
    );
    let mut done_count = 0;
    let mut queue = socket.inner.rx_queue.lock();
    while let Some(packet) = queue.pop_front() {
        let header = NlMsgHeader::read(&packet.data).unwrap();
        if header.msg_type == NLMSG_DONE {
            done_count += 1;
            let seq = header.seq;
            assert_eq!(seq, 43);
        }
    }
    // A truncated trailer is not an attribute error and must not suppress the
    // dump response or its DONE terminator.
    assert_eq!(done_count, 1);
}

#[def_test(serial)]
fn test_route_dump_stops_at_invalid_trailing_header() {
    init_test_state();
    let socket = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 42, groups: 0 }))
        .unwrap();

    let mut request = build_nlmsg(
        RTM_GETROUTE,
        44,
        NLM_F_REQUEST | NLM_F_DUMP,
        vec![wire_route::FAMILY_IPV4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    );
    // A second "header" whose nlmsg_len exceeds the remaining bytes.
    request.extend_from_slice(&64u32.to_ne_bytes());
    request.extend_from_slice(&[0xff; 12]);

    assert_eq!(
        socket.send(Cursor::new(request.as_slice()), SendOptions::default()),
        Ok(request.len())
    );
    let mut done_count = 0;
    let mut queue = socket.inner.rx_queue.lock();
    while let Some(packet) = queue.pop_front() {
        let header = NlMsgHeader::read(&packet.data).unwrap();
        if header.msg_type == NLMSG_DONE {
            done_count += 1;
            let seq = header.seq;
            assert_eq!(seq, 44);
        }
    }
    assert_eq!(done_count, 1);
}

#[def_test(serial)]
fn test_interface_ipv4_addr_survives_middle_device_removal() {
    let ethernet = |ifindex: i32, name: &str, id: u64| {
        TestDevice::new(LinkSnapshot {
            ifindex,
            name: String::from(name),
            flags: LINK_FLAG_UP
                | LINK_FLAG_RUNNING
                | LINK_FLAG_BROADCAST
                | LINK_FLAG_MULTICAST
                | LINK_FLAG_LOWER_UP,
            mtu: 1500,
            operstate: IF_OPER_UP,
            kind: LinkKind::Ethernet,
            hardware_addr: [0x02, 0, 0, 0, 0, ifindex as u8],
            broadcast_addr: [0xff; 6],
        })
        .with_device_id(kdevice::DeviceId::new(id))
    };
    let mut router = Router::new();
    router
        .add_device(kalloc_types::KBox::new(TestDevice::new(LinkSnapshot {
            ifindex: 1,
            name: String::from("lo"),
            flags: LINK_FLAG_UP | LINK_FLAG_RUNNING | LINK_FLAG_LOOPBACK | LINK_FLAG_LOWER_UP,
            mtu: 65_536,
            operstate: IF_OPER_UNKNOWN,
            kind: LinkKind::Loopback,
            hardware_addr: [0; 6],
            broadcast_addr: [0; 6],
        })).unwrap())
        .unwrap();
    router.add_device(kalloc_types::KBox::new(ethernet(2, "eth0", 2)).unwrap()).unwrap();
    router.add_device(kalloc_types::KBox::new(ethernet(3, "eth1", 3)).unwrap()).unwrap();
    for (dev, addr, scope) in [
        (
            0,
            crate::ip::Ipv4Address::new(127, 0, 0, 1),
            wire_route::SCOPE_HOST,
        ),
        (
            1,
            crate::ip::Ipv4Address::new(192, 168, 1, 2),
            wire_route::SCOPE_UNIVERSE,
        ),
        (
            2,
            crate::ip::Ipv4Address::new(10, 0, 0, 2),
            wire_route::SCOPE_UNIVERSE,
        ),
    ] {
        router
            .add_ipv4_addr(
                NetDeviceId::new(dev + 1).unwrap(),
                Ipv4AddrEntry {
                    addr: valid_ipv4_cidr(addr, 24),
                    scope,
                    broadcast: None,
                    no_prefix_route: false,
                    label: None,
                },
            )
            .unwrap();
    }
    if SERVICE.is_inited() {
        SERVICE.replace_router_for_tests(router);
    } else {
        SERVICE.init_once(Service::new(router));
    }

    // Removing eth0 preserves eth1's interface ID and address.
    assert!(crate::unregister_netdev_for_test(kdevice::DeviceId::new(2)));
    assert_eq!(
        crate::interface_ipv4_addr("eth1"),
        Ok(core::net::Ipv4Addr::new(10, 0, 0, 2))
    );
    assert_eq!(crate::interface_ipv4_addr("eth0"), Err(LinuxError::ENODEV));
    assert_eq!(crate::find_interface("eth1").unwrap().ifindex, 3);
    assert_eq!(
        crate::send_link_frame(2, &[]),
        Err(LinuxError::ENODEV.into())
    );
}

#[def_test]
fn test_device_removal_releases_address_capacity_and_preserves_surviving_ids() {
    let ethernet = |name: &str, model_id: u64| {
        TestDevice::new(LinkSnapshot {
            ifindex: 0,
            name: String::from(name),
            flags: LINK_FLAG_UP,
            mtu: 1500,
            operstate: IF_OPER_UP,
            kind: LinkKind::Ethernet,
            hardware_addr: [0; 6],
            broadcast_addr: [0xff; 6],
        })
        .with_device_id(kdevice::DeviceId::new(model_id))
    };
    let mut router = Router::new();
    let eth0 = router.add_device(kalloc_types::KBox::new(ethernet("eth0", 2)).unwrap()).unwrap();
    let eth1 = router.add_device(kalloc_types::KBox::new(ethernet("eth1", 3)).unwrap()).unwrap();
    let address = crate::ip::Ipv4Address::new(10, 0, 0, 2);
    let entry = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(address, 24),
        scope: wire_route::SCOPE_UNIVERSE,
        broadcast: None,
        no_prefix_route: false,
        label: None,
    };
    router.add_ipv4_addr(eth1, entry).unwrap();
    // Fill the shared capacity using another device's independent address set.
    for index in 1..smoltcp::config::IFACE_MAX_ADDR_COUNT {
        router
            .add_ipv4_addr(
                eth0,
                Ipv4AddrEntry {
                    addr: valid_ipv4_cidr(crate::ip::Ipv4Address::new(192, 0, 2, index as u8), 32),
                    ..entry
                },
            )
            .unwrap();
    }
    assert_eq!(router.add_ipv4_addr(eth1, entry), Ok(()));
    let extra = Ipv4AddrEntry {
        addr: valid_ipv4_cidr(crate::ip::Ipv4Address::new(198, 51, 100, 1), 24),
        ..entry
    };
    assert_eq!(router.add_ipv4_addr(eth1, extra), Err(LinuxError::ENOBUFS));
    assert_eq!(router.ipv4_addr_entries(eth1), &[entry]);

    router
        .remove_device_by_model_id(kdevice::DeviceId::new(2))
        .unwrap();
    assert_eq!(router.local_ipv4_addrs(), vec![entry.addr]);
    assert!(router.devices.ipv4(eth0).is_none());
    assert_eq!(router.add_ipv4_addr(eth0, extra), Err(LinuxError::ENODEV));
    let destination = crate::ip::IpAddress::Ipv4(crate::ip::Ipv4Address::new(10, 0, 0, 3));
    assert_eq!(
        router.output_route_source(&destination, eth1.ifindex()),
        Ok(address.into())
    );
    assert_eq!(
        router.output_route_source(&destination, eth0.ifindex()),
        Err(LinuxError::ENETUNREACH.into())
    );

    let eth2 = router.add_device(kalloc_types::KBox::new(ethernet("eth2", 4)).unwrap()).unwrap();
    assert_eq!(eth2.ifindex(), 4);
    assert!(!router.devices.has_device(eth0.ifindex()));
    assert!(router.ipv4_addr_entries(eth2).is_empty());
    assert_eq!(router.add_ipv4_addr(eth2, entry), Ok(()));
    assert_eq!(router.ipv4_addr_entries(eth1), &[entry]);
    router
        .remove_device_by_model_id(kdevice::DeviceId::new(3))
        .unwrap();
    assert!(router.has_ipv4_address(address));
    assert_eq!(router.ipv4_addr_entries(eth2), &[entry]);
    router
        .remove_device_by_model_id(kdevice::DeviceId::new(4))
        .unwrap();
    assert!(!router.has_ipv4_address(address));
    assert!(router.local_ipv4_addrs().is_empty());
    assert!(router.table.snapshot().is_empty());
}

#[def_test(serial)]
fn test_empty_rtnetlink_payload_is_ignored_before_privilege_and_validation() {
    init_test_state();
    let socket = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 40, groups: 0 }))
        .unwrap();
    let no_ack = build_nlmsg(RTM_NEWLINK, 40, NLM_F_REQUEST, Vec::new());
    assert_eq!(
        socket.send_with_cred(
            Cursor::new(no_ack.as_slice()),
            SendOptions::default(),
            &Cred::new(1000, 1000),
        ),
        Ok(no_ack.len())
    );
    assert!(socket.inner.rx_queue.lock().is_empty());

    let ack = build_nlmsg(RTM_NEWLINK, 41, NLM_F_REQUEST | NLM_F_ACK, Vec::new());
    assert_eq!(
        socket.send_with_cred(
            Cursor::new(ack.as_slice()),
            SendOptions::default(),
            &Cred::new(1000, 1000),
        ),
        Ok(ack.len())
    );
    let response = pop_event(&socket);
    assert_eq!(read_i32_ne(&response.data, NLMSG_HDR_LEN), Some(0));
}

#[def_test(serial)]
fn test_route_mutation_checks_each_send_credential() {
    init_test_state();
    let socket = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 42, groups: 0 }))
        .unwrap();
    let mutation = build_nlmsg(
        RTM_NEWLINK,
        41,
        NLM_F_REQUEST | NLM_F_ACK,
        link_dump_payload(&[]),
    );
    let unprivileged = Cred::new(1000, 1000);
    let privileged = Cred::root();

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

    // Queries need no capability.
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
            &privileged,
        ),
        Ok(mutation.len())
    );
    let invalid_request = socket.inner.rx_queue.lock().pop_front().unwrap();
    assert_eq!(
        read_i32_ne(&invalid_request.data, NLMSG_HDR_LEN),
        Some(-(LinuxError::ENODEV.into_raw()))
    );

    // An in-range unsupported non-GET operation is rejected by the capability
    // gate before handler lookup; an out-of-range type is rejected first and
    // independently of credentials.
    let set_link = build_nlmsg(RTM_SETLINK, 43, NLM_F_REQUEST, link_dump_payload(&[]));
    assert_eq!(
        socket.send_with_cred(
            Cursor::new(set_link.as_slice()),
            SendOptions::default(),
            &unprivileged,
        ),
        Ok(set_link.len())
    );
    let permission_error = socket.inner.rx_queue.lock().pop_front().unwrap();
    assert_eq!(
        read_i32_ne(&permission_error.data, NLMSG_HDR_LEN),
        Some(-(LinuxError::EPERM.into_raw()))
    );
    let out_of_range = build_nlmsg(RTM_MAX + 1, 44, NLM_F_REQUEST, vec![0]);
    assert_eq!(
        socket.send_with_cred(
            Cursor::new(out_of_range.as_slice()),
            SendOptions::default(),
            &unprivileged,
        ),
        Ok(out_of_range.len())
    );
    let unsupported = socket.inner.rx_queue.lock().pop_front().unwrap();
    assert_eq!(
        read_i32_ne(&unsupported.data, NLMSG_HDR_LEN),
        Some(-(LinuxError::EOPNOTSUPP.into_raw()))
    );
}

#[def_test(serial)]
fn test_route_socket_processes_same_class_mutation_batch() {
    init_test_state();
    let socket = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
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
    // Linux acknowledges back to the sending port, not to the destination
    // port encoded in the request's own `nlmsg_pid`.
    let first_pid = NlMsgHeader::read(&first_ack.data).unwrap().pid;
    let second_pid = NlMsgHeader::read(&second_ack.data).unwrap().pid;
    assert_eq!((first_pid, second_pid), (43, 43));
    assert!(queue.pop_front().is_none());
    drop(queue);

    assert!(SERVICE.ipv4_addr_entries().iter().any(|(dev, entry)| {
        entry.addr.address() == first_addr.into() && *dev == NetDeviceId::new(2).unwrap()
    }));
    assert!(SERVICE.ipv4_addr_entries().iter().any(|(dev, entry)| {
        entry.addr.address() == second_addr.into() && *dev == NetDeviceId::new(2).unwrap()
    }));
}

#[def_test(serial)]
fn test_route_socket_processes_query_batch() {
    init_test_state();
    let socket = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 47, groups: 0 }))
        .unwrap();
    let mut datagram = build_nlmsg(
        RTM_GETLINK,
        57,
        NLM_F_REQUEST | NLM_F_DUMP,
        link_dump_payload(&[]),
    );
    datagram.extend_from_slice(&build_nlmsg(
        RTM_GETADDR,
        58,
        NLM_F_REQUEST | NLM_F_DUMP,
        addr_dump_payload(),
    ));

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
        let pid = header.pid;
        assert_eq!(pid, 47, "reply must target the requesting port");
        if header.msg_type == NLMSG_DONE {
            done_sequences.push(header.seq);
        }
    }
    assert_eq!(done_sequences, vec![57, 58]);
}

#[def_test(serial)]
fn test_route_socket_processes_query_and_mutation_in_one_batch() {
    init_test_state();
    let socket = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
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

    // `netlink_rcv_skb` dispatches every complete message.  A query does not
    // authorize or veto a following mutation; each message carries its own
    // privilege check and result.
    assert_eq!(
        socket.send_with_cred(
            Cursor::new(datagram.as_slice()),
            SendOptions::default(),
            &Cred::root(),
        ),
        Ok(datagram.len())
    );
    assert!(SERVICE.ipv4_addr_entries().iter().any(|(dev, entry)| {
        entry.addr.address() == address.into() && *dev == NetDeviceId::new(2).unwrap()
    }));
    let mut acks = Vec::new();
    let mut queue = socket.inner.rx_queue.lock();
    while let Some(packet) = queue.pop_front() {
        let header = NlMsgHeader::read(&packet.data).unwrap();
        if header.msg_type == NLMSG_ERROR && header.seq == 60 {
            acks.push(read_i32_ne(&packet.data, NLMSG_HDR_LEN).unwrap());
        }
    }
    assert_eq!(acks, vec![0]);
}

#[def_test(serial)]
fn test_malformed_nlmsg_len_does_not_mutate_network_state() {
    init_test_state();
    let socket = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 44, groups: 0 }))
        .unwrap();
    let address = Ipv4Address::new(192, 168, 9, 79);
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
        Ok(request.len())
    );
    assert!(socket.inner.rx_queue.lock().is_empty());
    assert!(!SERVICE.ipv4_addr_entries().iter().any(|(dev, entry)| {
        entry.addr.address() == address.into() && *dev == NetDeviceId::new(2).unwrap()
    }));
}

fn group_bit(group: u32) -> u32 {
    1u32 << (group - 1)
}

fn event_header(packet: &NetlinkPacket) -> (u16, u32, u16) {
    let header = NlMsgHeader::read(&packet.data).expect("netlink event header");
    (header.msg_type, header.seq, header.flags)
}

fn event_pid(packet: &NetlinkPacket) -> u32 {
    NlMsgHeader::read(&packet.data)
        .expect("netlink event header")
        .pid
}

fn pop_event(socket: &NetlinkSocket) -> NetlinkPacket {
    socket
        .inner
        .rx_queue
        .lock()
        .pop_front()
        .expect("netlink event")
}

fn build_link_update(index: i32, flags: u32, change: u32) -> Vec<u8> {
    build_link_update_with(index, flags, change, 71, NLM_F_REQUEST | NLM_F_ACK)
}

fn build_link_update_with(
    index: i32,
    flags: u32,
    change: u32,
    seq: u32,
    msg_flags: u16,
) -> Vec<u8> {
    let mut payload = Vec::new();
    push_u8(&mut payload, 0);
    push_u8(&mut payload, 0);
    push_u16_ne(&mut payload, 1);
    push_i32_ne(&mut payload, index);
    push_u32_ne(&mut payload, flags);
    push_u32_ne(&mut payload, change);
    build_nlmsg(RTM_NEWLINK, seq, msg_flags, payload)
}

fn build_neigh_delete(family: u8, index: i32, dst: &[u8], seq: u32) -> Vec<u8> {
    let mut payload = Vec::new();
    push_u8(&mut payload, family);
    push_u8(&mut payload, 0);
    push_u16_ne(&mut payload, 0);
    push_i32_ne(&mut payload, index);
    push_u16_ne(&mut payload, 0);
    push_u8(&mut payload, 0);
    push_u8(&mut payload, 0);
    payload.extend(attr(wire_neigh::attr::DST, dst));
    build_nlmsg(RTM_DELNEIGH, seq, NLM_F_REQUEST | NLM_F_ACK, payload)
}

fn assert_netlink_error(socket: &NetlinkSocket, expected: LinuxError) {
    let packet = pop_event(socket);
    assert_eq!(event_header(&packet).0, NLMSG_ERROR);
    assert_eq!(
        read_i32_ne(&packet.data, NLMSG_HDR_LEN),
        Some(-expected.into_raw())
    );
}

#[def_test(serial)]
fn test_route_socket_unicast_requires_a_bound_destination() {
    let socket = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 45,
            groups: group_bit(RTNLGRP_LINK),
        }))
        .unwrap();
    let query = build_nlmsg(RTM_GETLINK, 54, NLM_F_REQUEST, Vec::new());

    // A unicast destination that is not bound is reported to the sender.
    assert_eq!(
        socket.send_with_cred(
            Cursor::new(query.as_slice()),
            SendOptions {
                to: Some(SocketAddrEx::Netlink(NetlinkAddr {
                    pid: 1234,
                    groups: 0,
                })),
                ..SendOptions::default()
            },
            &Cred::root(),
        ),
        Err(LinuxError::ECONNREFUSED.into())
    );
    assert!(socket.inner.rx_queue.lock().is_empty());

    let destination = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    destination
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 1234,
            groups: 0,
        }))
        .unwrap();
    let multicast = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    multicast
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 46,
            groups: group_bit(RTNLGRP_LINK),
        }))
        .unwrap();

    // A nonzero port ID consumes the message for userspace: it is not also
    // processed as a kernel request.
    assert_eq!(
        socket.send_with_cred(
            Cursor::new(query.as_slice()),
            SendOptions {
                to: Some(SocketAddrEx::Netlink(NetlinkAddr {
                    pid: 1234,
                    groups: 0,
                })),
                ..SendOptions::default()
            },
            &Cred::root(),
        ),
        Ok(query.len())
    );
    let packet = pop_event(&destination);
    assert_eq!(packet.from, NetlinkAddr { pid: 45, groups: 0 });
    assert_eq!(packet.data, query);
    assert!(socket.inner.rx_queue.lock().is_empty());
    assert!(multicast.inner.rx_queue.lock().is_empty());

    // A mixed send broadcasts the lowest selected group to subscribers other
    // than the sender and the unicast destination, then unicasts one copy
    // carrying that group in its source address.
    assert_eq!(
        socket.send_with_cred(
            Cursor::new(query.as_slice()),
            SendOptions {
                to: Some(SocketAddrEx::Netlink(NetlinkAddr {
                    pid: 1234,
                    groups: group_bit(RTNLGRP_LINK)
                        | group_bit(RTNLGRP_IPV4_IFADDR)
                        | group_bit(RTNLGRP_IPV4_ROUTE),
                })),
                ..SendOptions::default()
            },
            &Cred::root(),
        ),
        Ok(query.len())
    );
    let broadcast = pop_event(&multicast);
    assert_eq!(
        broadcast.from,
        NetlinkAddr {
            pid: 45,
            groups: group_bit(RTNLGRP_LINK),
        }
    );
    assert_eq!(broadcast.data, query);
    let unicast = pop_event(&destination);
    assert_eq!(
        unicast.from,
        NetlinkAddr {
            pid: 45,
            groups: group_bit(RTNLGRP_LINK),
        }
    );
    assert_eq!(unicast.data, query);
    assert!(socket.inner.rx_queue.lock().is_empty());
}

#[def_test(serial)]
fn test_route_unicast_reports_permission_lookup_and_queue_failures() {
    let unbound = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    let query = build_nlmsg(RTM_GETLINK, 55, NLM_F_REQUEST, Vec::new());

    // The permission check happens before autobind, so a rejected send leaves
    // the socket unbound.
    assert_eq!(
        unbound.send_with_cred(
            Cursor::new(query.as_slice()),
            SendOptions {
                to: Some(SocketAddrEx::Netlink(NetlinkAddr {
                    pid: 1234,
                    groups: 0,
                })),
                ..SendOptions::default()
            },
            &Cred::new(1000, 1000),
        ),
        Err(LinuxError::EPERM.into())
    );
    assert_eq!(
        unbound.local_addr().unwrap().into_netlink().unwrap(),
        NetlinkAddr::default()
    );

    // A privileged send autobinds before discovering that the destination is
    // missing, matching the ordering in netlink_sendmsg.
    assert_eq!(
        unbound.send_with_cred(
            Cursor::new(query.as_slice()),
            SendOptions {
                to: Some(SocketAddrEx::Netlink(NetlinkAddr {
                    pid: 1234,
                    groups: 0,
                })),
                ..SendOptions::default()
            },
            &Cred::root(),
        ),
        Err(LinuxError::ECONNREFUSED.into())
    );
    assert_ne!(unbound.local_addr().unwrap().into_netlink().unwrap().pid, 0);

    let destination = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    destination
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 1235,
            groups: 0,
        }))
        .unwrap();
    destination.inner.rx_queue.lock().push_back(NetlinkPacket {
        from: NetlinkAddr::default(),
        data: vec![0; NETLINK_RX_QUEUE_LIMIT + 1],
        credentials: None,
    });
    assert_eq!(
        unbound.send_with_cred(
            Cursor::new(query.as_slice()),
            SendOptions {
                to: Some(SocketAddrEx::Netlink(NetlinkAddr {
                    pid: 1235,
                    groups: 0,
                })),
                flags: crate::SendFlags::DONT_WAIT,
                ..SendOptions::default()
            },
            &Cred::root(),
        ),
        Err(LinuxError::EAGAIN.into())
    );

    // A blocking sender sleeps until the receiver frees enough space, then
    // completes the transfer.  The queued datagram exceeds the limit, so no
    // non-empty admission can fit the query; only fully draining the queue
    // restores the empty-queue exception.  `sleep` lets the spawned sender
    // reach the capacity wait before the receiver drains the queue.
    let destination = alloc::sync::Arc::new(destination);
    let sender = alloc::sync::Arc::new(unbound);
    let sender_for_task = sender.clone();
    let send_task = ktask::spawn(move || {
        let result = sender_for_task.send_with_cred(
            Cursor::new(query.as_slice()),
            SendOptions {
                to: Some(SocketAddrEx::Netlink(NetlinkAddr {
                    pid: 1235,
                    groups: 0,
                })),
                ..SendOptions::default()
            },
            &Cred::root(),
        );
        assert_eq!(result, Ok(query.len()));
    });
    ktask::sleep(ktime_types::TimeSpan::from_millis(1));
    assert_eq!(destination.inner.rx_queue.lock().packets.len(), 1);

    let mut drained = vec![0_u8; NETLINK_RX_QUEUE_LIMIT + 1];
    assert_eq!(
        destination.recv(&mut drained[..], RecvOptions::default()),
        Ok(NETLINK_RX_QUEUE_LIMIT + 1)
    );
    send_task.join();
    assert_eq!(destination.inner.rx_queue.lock().packets.len(), 1);
}

#[def_test(serial)]
fn test_route_unicast_to_dropped_receiver_is_refused_and_port_can_rebind() {
    let destination = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    destination
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 1236,
            groups: 0,
        }))
        .unwrap();

    // `netlink_release` removes the socket from the port table before waking
    // waiters.  A fresh send therefore finds no destination and reports
    // ECONNREFUSED.
    drop(destination);
    let sender = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    let query = build_nlmsg(RTM_GETLINK, 56, NLM_F_REQUEST, Vec::new());
    assert_eq!(
        sender.send_with_cred(
            Cursor::new(query.as_slice()),
            SendOptions {
                to: Some(SocketAddrEx::Netlink(NetlinkAddr {
                    pid: 1236,
                    groups: 0,
                })),
                ..SendOptions::default()
            },
            &Cred::root(),
        ),
        Err(LinuxError::ECONNREFUSED.into())
    );

    // Releasing the port also makes the identifier immediately reusable.
    let replacement = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    replacement
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 1236,
            groups: 0,
        }))
        .unwrap();
    assert_eq!(
        sender.send_with_cred(
            Cursor::new(query.as_slice()),
            SendOptions {
                to: Some(SocketAddrEx::Netlink(NetlinkAddr {
                    pid: 1236,
                    groups: 0,
                })),
                ..SendOptions::default()
            },
            &Cred::root(),
        ),
        Ok(query.len())
    );
    assert_eq!(pop_event(&replacement).data, query);
}

#[def_test(serial)]
fn test_route_blocked_unicast_retries_after_receiver_is_dropped() {
    let destination = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    destination
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 1237,
            groups: 0,
        }))
        .unwrap();
    assert!(destination.inner.rx_queue.lock().push_back(NetlinkPacket {
        from: NetlinkAddr::default(),
        data: vec![0; NETLINK_RX_QUEUE_LIMIT + 1],
        credentials: None,
    }));

    let sender = alloc::sync::Arc::new(NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw));
    let sender_for_task = sender.clone();
    let send_task = ktask::spawn(move || {
        let query = build_nlmsg(RTM_GETLINK, 57, NLM_F_REQUEST, Vec::new());
        let result = sender_for_task.send_with_cred(
            Cursor::new(query.as_slice()),
            SendOptions {
                to: Some(SocketAddrEx::Netlink(NetlinkAddr {
                    pid: 1237,
                    groups: 0,
                })),
                ..SendOptions::default()
            },
            &Cred::root(),
        );
        assert_eq!(result, Err(LinuxError::ECONNREFUSED.into()));
    });
    ktask::sleep(ktime_types::TimeSpan::from_millis(1));

    // Closing the receiver wakes `netlink_attachskb`, which asks the sender to
    // retry port lookup.  The port is already gone by the time waiters run.
    drop(destination);
    send_task.join();
}

#[def_test(serial)]
fn test_dropped_multicast_subscriber_no_longer_receives_events() {
    let subscriber = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    subscriber
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 1238,
            groups: group_bit(RTNLGRP_LINK),
        }))
        .unwrap();
    let sender = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    sender
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 1239,
            groups: 0,
        }))
        .unwrap();
    drop(subscriber);

    let request = build_link_update(2, 0, LINK_FLAG_UP);
    assert_eq!(
        sender.send_with_cred(
            Cursor::new(request.as_slice()),
            SendOptions {
                to: Some(SocketAddrEx::Netlink(NetlinkAddr {
                    pid: 0,
                    groups: group_bit(RTNLGRP_LINK),
                })),
                ..SendOptions::default()
            },
            &Cred::root(),
        ),
        Ok(request.len())
    );

    let replacement = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    replacement
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 1238,
            groups: group_bit(RTNLGRP_LINK),
        }))
        .unwrap();
    assert!(replacement.inner.rx_queue.lock().is_empty());
}

#[def_test(serial)]
fn test_zero_length_send_is_rejected_before_autobind() {
    let socket = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);

    // Linux rejects an empty datagram before autobind and before inspecting
    // the destination, so this reports ENODATA rather than ECONNREFUSED.
    assert_eq!(
        socket.send_with_cred(
            Cursor::new(&[][..]),
            SendOptions {
                to: Some(SocketAddrEx::Netlink(NetlinkAddr {
                    pid: 4321,
                    groups: 0,
                })),
                ..SendOptions::default()
            },
            &Cred::root(),
        ),
        Err(LinuxError::ENODATA.into())
    );
    assert_eq!(
        socket.local_addr().unwrap().into_netlink().unwrap(),
        NetlinkAddr::default()
    );
}

#[def_test(serial)]
fn test_netlink_out_of_band_is_rejected_before_other_semantics() {
    let socket = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);

    // Linux checks MSG_OOB before the empty-datagram check, destination
    // parsing, and implicit bind.  The destination below is deliberately
    // invalid so the test distinguishes EOPNOTSUPP from the later errors.
    assert_eq!(
        socket.send_with_cred(
            Cursor::new(&[][..]),
            SendOptions {
                to: Some(SocketAddrEx::Netlink(NetlinkAddr {
                    pid: 0,
                    groups: group_bit(RTNLGRP_LINK),
                })),
                flags: crate::SendFlags::OOB,
                ..SendOptions::default()
            },
            &Cred::new(1000, 1000),
        ),
        Err(LinuxError::EOPNOTSUPP.into())
    );
    assert_socket_unbound(&socket);

    let query = build_nlmsg(RTM_GETLINK, 57, NLM_F_REQUEST, Vec::new());
    assert_eq!(
        socket.send_with_cred(
            Cursor::new(query.as_slice()),
            SendOptions {
                to: Some(SocketAddrEx::Netlink(NetlinkAddr {
                    pid: 9999,
                    groups: 0,
                })),
                flags: crate::SendFlags::OOB,
                ..SendOptions::default()
            },
            &Cred::root(),
        ),
        Err(LinuxError::EOPNOTSUPP.into())
    );
    assert_socket_unbound(&socket);

    let mut byte = [0_u8; 1];
    assert_eq!(
        socket.recv(
            &mut byte[..],
            RecvOptions {
                flags: RecvFlags::OOB,
                ..RecvOptions::default()
            }
        ),
        Err(LinuxError::EOPNOTSUPP.into())
    );
}

#[def_test(serial)]
fn test_oversized_send_reports_emsgsize_after_autobind() {
    let socket = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    let payload = vec![0u8; NETLINK_SNDBUF - NETLINK_SNDBUF_RESERVED + 1];

    // The size check follows autobind in Linux, so the failed send still
    // leaves the socket with a port ID.
    assert_eq!(
        socket.send(Cursor::new(payload.as_slice()), SendOptions::default()),
        Err(LinuxError::EMSGSIZE.into())
    );
    assert_ne!(socket.local_addr().unwrap().into_netlink().unwrap().pid, 0);
}

#[def_test(serial)]
fn test_send_buffer_option_controls_emsgsize() {
    let socket = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    let mut send_buffer = 0;
    socket
        .get_option(GetSocketOption::SendBuffer(&mut send_buffer))
        .unwrap();
    assert_eq!(send_buffer, NETLINK_SNDBUF);

    // Linux clamps the hint to `wmem_max`, doubles it, and applies
    // `SOCK_MIN_SNDBUF` to the effective `sk_sndbuf`.
    let requested = 4096;
    socket
        .set_option(SetSocketOption::SendBuffer(&requested))
        .unwrap();
    socket
        .get_option(GetSocketOption::SendBuffer(&mut send_buffer))
        .unwrap();
    assert_eq!(send_buffer, 8192);
    let payload = vec![0u8; 8192 - NETLINK_SNDBUF_RESERVED + 1];
    assert_eq!(
        socket.send(Cursor::new(payload.as_slice()), SendOptions::default()),
        Err(LinuxError::EMSGSIZE.into())
    );

    // A hint above the initial `wmem_default` must still be honored up to
    // `wmem_max`, because Linux doubles the clamped hint.
    let requested = 1 << 20;
    socket
        .set_option(SetSocketOption::SendBuffer(&requested))
        .unwrap();
    socket
        .get_option(GetSocketOption::SendBuffer(&mut send_buffer))
        .unwrap();
    assert_eq!(send_buffer, 2 << 20);

    let usersock = NetlinkSocket::new(NETLINK_USERSOCK, NetlinkSocketType::Raw);
    usersock
        .set_option(SetSocketOption::SendBuffer(&requested))
        .unwrap();
    let payload = vec![0u8; 500 * 1024];
    assert_eq!(
        usersock.send(Cursor::new(payload.as_slice()), SendOptions::default()),
        Err(LinuxError::ECONNREFUSED.into())
    );

    let requested = NETLINK_WMEM_MAX + 1;
    socket
        .set_option(SetSocketOption::SendBuffer(&requested))
        .unwrap();
    socket
        .get_option(GetSocketOption::SendBuffer(&mut send_buffer))
        .unwrap();
    assert_eq!(send_buffer, 2 * NETLINK_WMEM_MAX);

    let requested = 1;
    socket
        .set_option(SetSocketOption::SendBuffer(&requested))
        .unwrap();
    socket
        .get_option(GetSocketOption::SendBuffer(&mut send_buffer))
        .unwrap();
    assert_eq!(send_buffer, NETLINK_SNDBUF_MIN);
}

#[def_test(serial)]
fn test_receive_buffer_option_controls_queue_capacity() {
    let socket = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    let mut receive_buffer = 0;
    socket
        .get_option(GetSocketOption::ReceiveBuffer(&mut receive_buffer))
        .unwrap();
    assert_eq!(receive_buffer, NETLINK_RX_QUEUE_LIMIT);

    // Linux clamps the hint to `rmem_max`, doubles it, and applies
    // `SOCK_MIN_RCVBUF` to the effective `sk_rcvbuf`.
    let requested = 4096;
    socket
        .set_option(SetSocketOption::ReceiveBuffer(&requested))
        .unwrap();
    socket
        .get_option(GetSocketOption::ReceiveBuffer(&mut receive_buffer))
        .unwrap();
    assert_eq!(receive_buffer, 8192);
    let packet = |len| NetlinkPacket {
        from: NetlinkAddr::default(),
        data: vec![0; len],
        credentials: None,
    };
    // An empty queue admits one oversized datagram, matching Linux's
    // `rmem == skb->truesize` exception.  Once anything is queued, admission
    // includes the incoming datagram and an over-limit push is rejected.
    assert!(socket.inner.rx_queue.lock().push_back(packet(8192)));
    assert!(!socket.inner.rx_queue.lock().push_back(packet(1)));
    socket.inner.rx_queue.lock().pop_front();

    // A non-empty queue at exactly the limit has no room for any nonzero
    // datagram, while draining it restores the empty-queue exception.
    assert!(socket.inner.rx_queue.lock().push_back(packet(8191)));
    assert!(socket.inner.rx_queue.lock().push_back(packet(1)));
    assert!(!socket.inner.rx_queue.lock().push_back(packet(1)));
    socket.inner.rx_queue.lock().pop_front();
    socket.inner.rx_queue.lock().pop_front();

    let requested = 1;
    socket
        .set_option(SetSocketOption::ReceiveBuffer(&requested))
        .unwrap();
    socket
        .get_option(GetSocketOption::ReceiveBuffer(&mut receive_buffer))
        .unwrap();
    assert_eq!(receive_buffer, NETLINK_RCVBUF_MIN);

    let requested = NETLINK_RMEM_MAX + 1;
    socket
        .set_option(SetSocketOption::ReceiveBuffer(&requested))
        .unwrap();
    socket
        .get_option(GetSocketOption::ReceiveBuffer(&mut receive_buffer))
        .unwrap();
    assert_eq!(receive_buffer, 2 * NETLINK_RMEM_MAX);
    // The enlarged limit must admit a datagram larger than the 212992-byte
    // default, just as Linux does after a raised `SO_RCVBUF`.
    assert!(socket.inner.rx_queue.lock().push_back(packet(512 * 1024)));
    socket.inner.rx_queue.lock().pop_front();
}

#[def_test(serial)]
fn test_multicast_overrun_reports_so_error_and_drain_clears_congestion() {
    let socket = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 1236,
            groups: group_bit(RTNLGRP_LINK),
        }))
        .unwrap();
    let requested = 4096;
    socket
        .set_option(SetSocketOption::ReceiveBuffer(&requested))
        .unwrap();

    let packet = |len| NetlinkPacket {
        from: NetlinkAddr::default(),
        data: vec![0; len],
        credentials: None,
    };
    assert!(socket.inner.rx_queue.lock().push_back(packet(8192)));
    // A non-empty queue cannot admit even a one-byte datagram once the
    // incoming size is included in admission.
    assert!(!socket.inner.rx_queue.lock().push_back(packet(1)));
    publish_route_event(RTNLGRP_LINK, b"dropped");

    // First overrun transition stores a positive ENOBUFS and reports it as a
    // pending socket error.  SO_ERROR consumes the error without changing the
    // congestion state.
    let mut error = 0;
    socket
        .get_option(GetSocketOption::Error(&mut error))
        .unwrap();
    assert_eq!(error, LinuxError::ENOBUFS.into_raw());
    assert!(Pollable::poll(&socket).contains(IoEvents::IN));
    assert!(!Pollable::poll(&socket).contains(IoEvents::ERR));
    socket
        .get_option(GetSocketOption::Error(&mut error))
        .unwrap();
    assert_eq!(error, 0);

    // Congestion persists until the receive queue is drained; then the socket
    // can accept multicast again.
    let mut buffer = vec![0; 8192];
    assert_eq!(
        socket.recv(&mut buffer[..], RecvOptions::default()),
        Ok(8192)
    );
    assert!(socket.inner.rx_queue.lock().is_empty());
    publish_route_event(RTNLGRP_LINK, b"after drain");
    assert!(Pollable::poll(&socket).contains(IoEvents::IN));
    assert_eq!(
        socket.inner.rx_queue.lock().pop_front().unwrap().data,
        b"after drain"
    );
}

#[def_test(serial)]
fn test_poll_registers_receive_source_for_error_and_readable_waiters() {
    let socket = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    let context = Context::from_waker(Waker::noop());
    let mut registrations = PollRegistrations::new();

    // Linux registers the receive wait queue before computing readiness, so
    // both an already-readable socket and an error-only waiter must own a
    // receive registration.
    Pollable::register(&socket, &mut registrations.context(&context), IoEvents::ERR).unwrap();
    assert_eq!(registrations.len(), 1);
    Pollable::register(&socket, &mut registrations.context(&context), IoEvents::IN).unwrap();
    assert_eq!(registrations.len(), 1);
    Pollable::register(&socket, &mut registrations.context(&context), IoEvents::OUT).unwrap();
    assert!(registrations.is_empty());
}

#[def_test(serial)]
fn test_poll_reports_pending_netlink_error() {
    let socket = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 1238,
            groups: group_bit(RTNLGRP_LINK),
        }))
        .unwrap();
    let requested = 4096;
    socket
        .set_option(SetSocketOption::ReceiveBuffer(&requested))
        .unwrap();
    assert!(socket.inner.rx_queue.lock().push_back(NetlinkPacket {
        from: NetlinkAddr::default(),
        data: vec![0; 8192],
        credentials: None,
    }));
    assert!(!socket.inner.rx_queue.lock().push_back(NetlinkPacket {
        from: NetlinkAddr::default(),
        data: vec![0; 1],
        credentials: None,
    }));
    publish_route_event(RTNLGRP_LINK, b"dropped");
    assert!(Pollable::poll(&socket).contains(IoEvents::ERR));

    let mut error = 0;
    socket
        .get_option(GetSocketOption::Error(&mut error))
        .unwrap();
    assert_eq!(error, LinuxError::ENOBUFS.into_raw());
    assert!(!Pollable::poll(&socket).contains(IoEvents::ERR));
}

#[def_test(serial)]
fn test_unicast_accepts_one_datagram_larger_than_receive_buffer() {
    let destination = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    destination
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 1237,
            groups: 0,
        }))
        .unwrap();
    let requested = 4096;
    destination
        .set_option(SetSocketOption::ReceiveBuffer(&requested))
        .unwrap();

    let sender = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    let big = vec![0x5a; 8193];
    // With one byte already queued, Linux's post-insert accounting rejects the
    // oversized datagram instead of admitting it against a merely non-empty
    // queue.  Draining the one byte empties the queue and restores the
    // one-oversized-datagram exception.
    assert_eq!(
        sender.send_with_cred(
            Cursor::new([0_u8].as_slice()),
            SendOptions {
                to: Some(SocketAddrEx::Netlink(NetlinkAddr {
                    pid: 1237,
                    groups: 0,
                })),
                flags: crate::SendFlags::DONT_WAIT,
                ..SendOptions::default()
            },
            &Cred::root(),
        ),
        Ok(1)
    );
    assert_eq!(
        sender.send_with_cred(
            Cursor::new(big.as_slice()),
            SendOptions {
                to: Some(SocketAddrEx::Netlink(NetlinkAddr {
                    pid: 1237,
                    groups: 0,
                })),
                flags: crate::SendFlags::DONT_WAIT,
                ..SendOptions::default()
            },
            &Cred::root(),
        ),
        Err(LinuxError::EAGAIN.into())
    );
    let mut one = [0_u8; 1];
    assert_eq!(
        destination.recv(&mut one[..], RecvOptions::default()),
        Ok(1)
    );
    assert_eq!(
        sender.send_with_cred(
            Cursor::new(big.as_slice()),
            SendOptions {
                to: Some(SocketAddrEx::Netlink(NetlinkAddr {
                    pid: 1237,
                    groups: 0,
                })),
                flags: crate::SendFlags::DONT_WAIT,
                ..SendOptions::default()
            },
            &Cred::root(),
        ),
        Ok(big.len())
    );

    // After the oversized datagram is queued, further packets are rejected
    // until the queue drains completely, because the queued total already
    // exceeds the limit.
    assert_eq!(
        sender.send_with_cred(
            Cursor::new([0_u8].as_slice()),
            SendOptions {
                to: Some(SocketAddrEx::Netlink(NetlinkAddr {
                    pid: 1237,
                    groups: 0,
                })),
                flags: crate::SendFlags::DONT_WAIT,
                ..SendOptions::default()
            },
            &Cred::root(),
        ),
        Err(LinuxError::EAGAIN.into())
    );

    let mut received = vec![0; big.len()];
    assert_eq!(
        destination.recv(&mut received[..], RecvOptions::default()),
        Ok(big.len())
    );
    assert_eq!(received, big);

    assert_eq!(
        sender.send_with_cred(
            Cursor::new([0_u8].as_slice()),
            SendOptions {
                to: Some(SocketAddrEx::Netlink(NetlinkAddr {
                    pid: 1237,
                    groups: 0,
                })),
                flags: crate::SendFlags::DONT_WAIT,
                ..SendOptions::default()
            },
            &Cred::root(),
        ),
        Ok(1)
    );
}

/// `NETLINK_ADD_MEMBERSHIP` links the socket into the protocol's broadcast
/// list (`mc_list`) without needing a port id, so a subscriber that never binds
/// still receives its group's multicast.
#[def_test(serial)]
fn test_unbound_group_subscriber_receives_multicast() {
    let subscriber = NetlinkSocket::new(NETLINK_KOBJECT_UEVENT, NetlinkSocketType::Raw);
    subscriber
        .set_option(SetSocketOption::NetlinkAddMembership(&1))
        .unwrap();
    assert!(
        subscriber.inner.local_addr.read().is_none(),
        "the subscriber never binds"
    );
    let sender = NetlinkSocket::new(NETLINK_KOBJECT_UEVENT, NetlinkSocketType::Raw);
    sender
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 631,
            groups: 0,
        }))
        .unwrap();

    let payload = b"ACTION=add\0";
    let send = |sender: &NetlinkSocket| {
        sender.send_with_cred(
            Cursor::new(payload.as_slice()),
            SendOptions {
                to: Some(SocketAddrEx::Netlink(NetlinkAddr { pid: 0, groups: 1 })),
                ..SendOptions::default()
            },
            &Cred::root(),
        )
    };
    assert_eq!(send(&sender), Ok(payload.len()));

    let packet = pop_event(&subscriber);
    assert_eq!(packet.data, payload);
    assert_eq!(packet.from.pid, 631);

    // Dropping the last subscription unlinks it from the broadcast list again.
    subscriber
        .set_option(SetSocketOption::NetlinkDropMembership(&1))
        .unwrap();
    assert_eq!(send(&sender), Ok(payload.len()));
    assert!(subscriber.inner.rx_queue.lock().is_empty());
}

/// The `CAP_NET_ADMIN` buffer forms resize the same netlink queues as the
/// unforced options: `sock_setsockopt` writes `sk_sndbuf`/`sk_rcvbuf` for both,
/// and netlink's accessors and admission checks read those fields.
#[def_test(serial)]
fn test_netlink_buffer_force_options_resize_the_queues() {
    let socket = NetlinkSocket::new(NETLINK_USERSOCK, NetlinkSocketType::Raw);

    let mut before = 0usize;
    socket
        .get_option(GetSocketOption::ReceiveBuffer(&mut before))
        .unwrap();
    let request = 65_536usize;
    socket
        .set_option(SetSocketOption::ReceiveBufferForce(&request))
        .unwrap();
    let mut after = 0usize;
    socket
        .get_option(GetSocketOption::ReceiveBuffer(&mut after))
        .unwrap();
    assert_eq!(after, request * 2, "the force form doubles the hint");
    assert_eq!(
        socket.inner.rx_queue.lock().limit(),
        after,
        "the receive queue follows the readback"
    );

    let send_request = 32_768usize;
    socket
        .set_option(SetSocketOption::SendBufferForce(&send_request))
        .unwrap();
    assert_eq!(
        socket.inner.send_buffer.load(Ordering::Relaxed),
        send_request * 2,
        "the send buffer follows the force form"
    );
}

#[def_test(serial)]
fn test_uevent_multicast_is_scoped_to_uevent_sockets() {
    let route_subscriber = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    route_subscriber
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 602,
            groups: group_bit(RTNLGRP_LINK),
        }))
        .unwrap();
    let uevent_subscriber = NetlinkSocket::new(NETLINK_KOBJECT_UEVENT, NetlinkSocketType::Raw);
    uevent_subscriber
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 603,
            groups: 1,
        }))
        .unwrap();
    let sender = NetlinkSocket::new(NETLINK_KOBJECT_UEVENT, NetlinkSocketType::Raw);
    sender
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 604,
            groups: 0,
        }))
        .unwrap();

    let payload = b"ACTION=add\0";
    let options = || SendOptions {
        to: Some(SocketAddrEx::Netlink(NetlinkAddr { pid: 0, groups: 1 })),
        ..SendOptions::default()
    };
    assert_eq!(
        sender.send_with_cred(Cursor::new(payload.as_slice()), options(), &Cred::root()),
        Ok(payload.len())
    );

    let packet = pop_event(&uevent_subscriber);
    assert_eq!(
        packet.from,
        NetlinkAddr {
            pid: 604,
            groups: 1,
        }
    );
    assert_eq!(packet.data, payload);
    assert!(route_subscriber.inner.rx_queue.lock().is_empty());

    // Uevent, like route, does not grant NL_CFG_F_NONROOT_SEND.
    assert_eq!(
        sender.send_with_cred(
            Cursor::new(payload.as_slice()),
            options(),
            &Cred::new(1000, 1000),
        ),
        Err(LinuxError::EPERM.into())
    );
    assert!(uevent_subscriber.inner.rx_queue.lock().is_empty());
}

#[def_test(serial)]
fn test_usersock_allows_unprivileged_user_datagrams_but_has_no_kernel_port() {
    let unprivileged = Cred::new(1000, 1000);
    let receiver = NetlinkSocket::new(NETLINK_USERSOCK, NetlinkSocketType::Raw);
    receiver
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 605,
            groups: 0,
        }))
        .unwrap();
    let sender = NetlinkSocket::new(NETLINK_USERSOCK, NetlinkSocketType::Raw);
    sender
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 606,
            groups: 0,
        }))
        .unwrap();
    let payload = b"usersock";

    // NETLINK_USERSOCK is the only supported protocol with
    // NL_CFG_F_NONROOT_SEND, so an unprivileged process may address another
    // user socket directly.
    assert_eq!(
        sender.send_with_cred(
            Cursor::new(payload.as_slice()),
            SendOptions {
                to: Some(SocketAddrEx::Netlink(NetlinkAddr {
                    pid: 605,
                    groups: 0,
                })),
                ..SendOptions::default()
            },
            &unprivileged,
        ),
        Ok(payload.len())
    );
    let packet = pop_event(&receiver);
    assert_eq!(
        packet.from,
        NetlinkAddr {
            pid: 606,
            groups: 0
        }
    );
    assert_eq!(packet.data, payload);

    // Group receive is still administrative: NL_CFG_F_NONROOT_RECV is not set
    // for NETLINK_USERSOCK.
    let subscriber = NetlinkSocket::new(NETLINK_USERSOCK, NetlinkSocketType::Raw);
    assert_eq!(
        subscriber.bind_with_cred(
            SocketAddrEx::Netlink(NetlinkAddr {
                pid: 607,
                groups: 1,
            }),
            &unprivileged,
        ),
        Err(LinuxError::EPERM.into())
    );
    subscriber
        .bind_with_cred(
            SocketAddrEx::Netlink(NetlinkAddr {
                pid: 607,
                groups: 1,
            }),
            &Cred::root(),
        )
        .unwrap();

    // Linux broadcasts before the final port-0 unicast.  The broadcast is
    // delivered, but NETLINK_USERSOCK has no kernel socket at port 0, so the
    // send itself still reports ECONNREFUSED.
    assert_eq!(
        sender.send_with_cred(
            Cursor::new(payload.as_slice()),
            SendOptions {
                to: Some(SocketAddrEx::Netlink(NetlinkAddr { pid: 0, groups: 1 })),
                ..SendOptions::default()
            },
            &unprivileged,
        ),
        Err(LinuxError::ECONNREFUSED.into())
    );
    let broadcast = pop_event(&subscriber);
    assert_eq!(
        broadcast.from,
        NetlinkAddr {
            pid: 606,
            groups: 1,
        }
    );
    assert_eq!(broadcast.data, payload);

    // A default destination is also a port-0 unicast.  The permission check
    // passes, autobind runs, and then the missing kernel socket is reported.
    let kernel_port_sender = NetlinkSocket::new(NETLINK_USERSOCK, NetlinkSocketType::Raw);
    assert_eq!(
        kernel_port_sender.send_with_cred(
            Cursor::new(payload.as_slice()),
            SendOptions::default(),
            &unprivileged,
        ),
        Err(LinuxError::ECONNREFUSED.into())
    );
    assert_ne!(
        kernel_port_sender
            .local_addr()
            .unwrap()
            .into_netlink()
            .unwrap()
            .pid,
        0
    );
}

#[def_test(serial)]
fn test_kobject_uevent_kernel_port_requires_cap_sys_admin() {
    let subscriber = NetlinkSocket::new(NETLINK_KOBJECT_UEVENT, NetlinkSocketType::Raw);
    subscriber
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 632,
            groups: 1,
        }))
        .unwrap();
    let sender = NetlinkSocket::new(NETLINK_KOBJECT_UEVENT, NetlinkSocketType::Raw);
    sender
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 630,
            groups: 0,
        }))
        .unwrap();
    let message = build_nlmsg(
        NLMSG_MIN_TYPE,
        70,
        NLM_F_REQUEST | NLM_F_ACK,
        b"ACTION=add\0".to_vec(),
    );

    // The port-0 receive callback requires CAP_SYS_ADMIN.  Its failure is an
    // ACKed netlink error, not a failure of the enclosing send.
    assert_eq!(
        sender.send_with_cred(
            Cursor::new(message.as_slice()),
            SendOptions::default(),
            &Cred::new(1000, 1000),
        ),
        Ok(message.len())
    );
    let response = pop_event(&sender);
    let header = NlMsgHeader::read(&response.data).unwrap();
    let (msg_type, seq, pid) = (header.msg_type, header.seq, header.pid);
    assert_eq!(msg_type, NLMSG_ERROR);
    assert_eq!(seq, 70);
    assert_eq!(pid, 630);
    assert_eq!(
        read_i32_ne(&response.data, NLMSG_HDR_LEN),
        Some(-(LinuxError::EPERM.into_raw()))
    );

    // A callback error is always reported, even without NLM_F_ACK.
    let message = build_nlmsg(NLMSG_MIN_TYPE, 71, NLM_F_REQUEST, b"ACTION=add\0".to_vec());
    assert_eq!(
        sender.send_with_cred(
            Cursor::new(message.as_slice()),
            SendOptions::default(),
            &Cred::new(1000, 1000),
        ),
        Ok(message.len())
    );
    let response = pop_event(&sender);
    assert_eq!(
        read_i32_ne(&response.data, NLMSG_HDR_LEN),
        Some(-(LinuxError::EPERM.into_raw()))
    );

    // `netlink_ns_capable()` also tests the opener for an addressless send.
    // A privileged sender using a socket opened by an unprivileged task must
    // not be able to forge a kernel-originated uevent.
    let message = build_nlmsg(
        NLMSG_MIN_TYPE,
        72,
        NLM_F_REQUEST | NLM_F_ACK,
        b"ACTION=forged\0".to_vec(),
    );
    assert_eq!(
        sender.send_with_cred(
            Cursor::new(message.as_slice()),
            SendOptions {
                opener_cred: Some(Cred::new(1000, 1000)),
                ..SendOptions::default()
            },
            &Cred::root(),
        ),
        Ok(message.len())
    );
    let response = pop_event(&sender);
    assert_eq!(
        read_i32_ne(&response.data, NLMSG_HDR_LEN),
        Some(-(LinuxError::EPERM.into_raw()))
    );
    assert!(subscriber.inner.rx_queue.lock().is_empty());
}

#[def_test(serial)]
fn test_kobject_uevent_callback_uses_cap_sys_admin() {
    let subscriber = NetlinkSocket::new(NETLINK_KOBJECT_UEVENT, NetlinkSocketType::Raw);
    subscriber
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 633,
            groups: 1,
        }))
        .unwrap();
    let sender = NetlinkSocket::new(NETLINK_KOBJECT_UEVENT, NetlinkSocketType::Raw);
    sender
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 634,
            groups: 0,
        }))
        .unwrap();

    let sys_admin = credential_with_capabilities(1000, capability(CAP_SYS_ADMIN));
    let next_seqnum = UEVENT_SEQNUM.load(Ordering::Relaxed) + 1;
    let message = build_nlmsg(
        NLMSG_MIN_TYPE,
        73,
        NLM_F_REQUEST,
        b"ACTION=capability\0".to_vec(),
    );
    assert_eq!(
        sender.send_with_cred(
            Cursor::new(message.as_slice()),
            SendOptions {
                opener_cred: Some(sys_admin.clone()),
                ..SendOptions::default()
            },
            &sys_admin,
        ),
        Ok(message.len())
    );
    let event = pop_event(&subscriber);
    let suffix = format!("SEQNUM={next_seqnum}\0");
    assert!(event.data.ends_with(suffix.as_bytes()));

    // CAP_NET_ADMIN authorizes rtnetlink, not the uevent receive callback.
    let net_admin = credential_with_capabilities(1000, capability(CAP_NET_ADMIN));
    let message = build_nlmsg(
        NLMSG_MIN_TYPE,
        74,
        NLM_F_REQUEST,
        b"ACTION=wrong-capability\0".to_vec(),
    );
    assert_eq!(
        sender.send_with_cred(
            Cursor::new(message.as_slice()),
            SendOptions {
                opener_cred: Some(net_admin.clone()),
                ..SendOptions::default()
            },
            &net_admin,
        ),
        Ok(message.len())
    );
    let response = pop_event(&sender);
    assert_eq!(
        read_i32_ne(&response.data, NLMSG_HDR_LEN),
        Some(-(LinuxError::EPERM.into_raw()))
    );
}

#[def_test(serial)]
fn test_kobject_uevent_kernel_port_rewrites_and_rebroadcasts() {
    let subscriber = NetlinkSocket::new(NETLINK_KOBJECT_UEVENT, NetlinkSocketType::Raw);
    subscriber
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 631,
            groups: 1,
        }))
        .unwrap();
    let sender = NetlinkSocket::new(NETLINK_KOBJECT_UEVENT, NetlinkSocketType::Raw);
    sender
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 632,
            groups: 0,
        }))
        .unwrap();
    let body = b"add@/devices/test\0ACTION=add\0SUBSYSTEM=block\0";
    let message = build_nlmsg(NLMSG_MIN_TYPE, 72, NLM_F_REQUEST | NLM_F_ACK, body.to_vec());

    assert_eq!(
        sender.send_with_cred(
            Cursor::new(message.as_slice()),
            SendOptions::default(),
            &Cred::root(),
        ),
        Ok(message.len())
    );

    // The receiving kernel path strips the netlink header, appends a trailing
    // NUL-terminated sequence number, and publishes as port 0 to group 1.
    let packet = pop_event(&subscriber);
    assert_eq!(packet.from, NetlinkAddr { pid: 0, groups: 1 });
    // The callback receives the whole remaining datagram, including the
    // alignment padding of this netlink message.  Only the header is pulled
    // before the sequence number is appended.
    let expected_prefix = &message[NLMSG_HDR_LEN..];
    assert!(packet.data.starts_with(expected_prefix));
    let suffix = &packet.data[expected_prefix.len()..];
    assert_eq!(suffix.last(), Some(&0));
    let suffix = core::str::from_utf8(&suffix[..suffix.len() - 1]).unwrap();
    assert!(suffix.starts_with("SEQNUM="));
    assert!(suffix[7..].bytes().all(|byte| byte.is_ascii_digit()));

    let ack = pop_event(&sender);
    let ack_header = NlMsgHeader::read(&ack.data).unwrap();
    let ack_type = ack_header.msg_type;
    assert_eq!(ack_type, NLMSG_ERROR);
    assert_eq!(read_i32_ne(&ack.data, NLMSG_HDR_LEN), Some(0));
}

#[def_test(serial)]
fn test_kobject_uevent_kernel_port_broadcasts_remaining_datagram_per_message() {
    let subscriber = NetlinkSocket::new(NETLINK_KOBJECT_UEVENT, NetlinkSocketType::Raw);
    subscriber
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 633,
            groups: 1,
        }))
        .unwrap();
    let sender = NetlinkSocket::new(NETLINK_KOBJECT_UEVENT, NetlinkSocketType::Raw);
    sender
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 634,
            groups: 0,
        }))
        .unwrap();
    let first = build_nlmsg(NLMSG_MIN_TYPE, 73, NLM_F_REQUEST, b"first=1\0".to_vec());
    let second = build_nlmsg(NLMSG_MIN_TYPE, 74, NLM_F_REQUEST, b"second=2\0".to_vec());
    let mut datagram = first.clone();
    datagram.extend_from_slice(&second);

    assert_eq!(
        sender.send_with_cred(
            Cursor::new(datagram.as_slice()),
            SendOptions::default(),
            &Cred::root(),
        ),
        Ok(datagram.len())
    );

    let first_event = pop_event(&subscriber);
    assert!(first_event.data.starts_with(&datagram[NLMSG_HDR_LEN..]));
    let second_start = first.len();
    let second_event = pop_event(&subscriber);
    assert!(
        second_event
            .data
            .starts_with(&datagram[second_start + NLMSG_HDR_LEN..])
    );
    assert!(subscriber.inner.rx_queue.lock().is_empty());
}

#[def_test(serial)]
fn test_kobject_uevent_kernel_port_rejects_oversized_datagram() {
    let sender = NetlinkSocket::new(NETLINK_KOBJECT_UEVENT, NetlinkSocketType::Raw);
    sender
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 635,
            groups: 0,
        }))
        .unwrap();
    let message = build_nlmsg(
        NLMSG_MIN_TYPE,
        75,
        NLM_F_REQUEST | NLM_F_ACK,
        vec![0; UEVENT_BUFFER_SIZE - NLMSG_HDR_LEN],
    );

    // The callback measures the whole remaining skb, including any bytes
    // after the current message, before appending SEQNUM.
    assert_eq!(
        sender.send_with_cred(
            Cursor::new(message.as_slice()),
            SendOptions::default(),
            &Cred::root(),
        ),
        Ok(message.len())
    );
    let response = pop_event(&sender);
    let header = NlMsgHeader::read(&response.data).unwrap();
    let msg_type = header.msg_type;
    assert_eq!(msg_type, NLMSG_ERROR);
    assert_eq!(
        read_i32_ne(&response.data, NLMSG_HDR_LEN),
        Some(-(LinuxError::EINVAL.into_raw()))
    );
}

#[def_test(serial)]
fn test_route_socket_binds_subscription_and_allocates_port_id() {
    let socket = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    let mask = group_bit(RTNLGRP_LINK) | group_bit(RTNLGRP_IPV4_IFADDR);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 0,
            groups: mask,
        }))
        .unwrap();
    let SocketAddrEx::Netlink(bound) = socket.local_addr().unwrap() else {
        panic!("netlink local address");
    };
    assert_ne!(bound.pid, 0);
    assert_eq!(bound.groups, mask);

    // Linux rejects a rebind that omits the current port ID; zero does not
    // mean "keep the existing ID".
    assert_eq!(
        socket.bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 0,
            groups: group_bit(RTNLGRP_IPV4_ROUTE),
        })),
        Err(LinuxError::EINVAL.into())
    );
    let SocketAddrEx::Netlink(unchanged) = socket.local_addr().unwrap() else {
        panic!("netlink local address");
    };
    assert_eq!(unchanged.pid, bound.pid);
    assert_eq!(unchanged.groups, mask);

    // Repeating the port ID updates the subscription mask in place.
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: bound.pid,
            groups: group_bit(RTNLGRP_IPV4_ROUTE),
        }))
        .unwrap();
    let SocketAddrEx::Netlink(rebound) = socket.local_addr().unwrap() else {
        panic!("netlink local address");
    };
    assert_eq!(rebound.pid, bound.pid);
    assert_eq!(rebound.groups, group_bit(RTNLGRP_IPV4_ROUTE));

    // A bound socket cannot be moved to a different port ID.
    assert_eq!(
        socket.bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: bound.pid + 1,
            groups: 0,
        })),
        Err(LinuxError::EINVAL.into())
    );

    let duplicate = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    assert_eq!(
        duplicate.bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: bound.pid,
            groups: 0,
        })),
        Err(LinuxError::EADDRINUSE.into())
    );

    // Dropping the owner makes the port ID available again.
    drop(socket);
    duplicate
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: bound.pid,
            groups: 0,
        }))
        .unwrap();
}

#[def_test(serial)]
fn test_reverse_path_route_groups_require_admin() {
    let unprivileged = Cred::new(1000, 1000);
    let socket = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    let reverse_path_groups = group_bit(RTNLGRP_IPV4_MROUTE_R) | group_bit(RTNLGRP_IPV6_MROUTE_R);

    // The credential-free trait method cannot claim administrative authority.
    assert_eq!(
        socket.bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 0,
            groups: reverse_path_groups,
        })),
        Err(LinuxError::EPERM.into())
    );
    assert_socket_unbound(&socket);

    assert_eq!(
        socket.bind_with_cred(
            SocketAddrEx::Netlink(NetlinkAddr {
                pid: 0,
                groups: reverse_path_groups,
            }),
            &unprivileged,
        ),
        Err(LinuxError::EPERM.into())
    );
    assert_socket_unbound(&socket);

    socket
        .bind_with_cred(
            SocketAddrEx::Netlink(NetlinkAddr {
                pid: 0,
                groups: reverse_path_groups,
            }),
            &Cred::root(),
        )
        .unwrap();
    let SocketAddrEx::Netlink(bound) = socket.local_addr().unwrap() else {
        panic!("netlink local address");
    };
    assert_eq!(bound.groups, reverse_path_groups);

    // Generic port-ID conformance runs before `rtnetlink_bind`'s per-group
    // callback, so a rebind to another port fails with EINVAL even when the
    // requested group would otherwise require privilege.
    assert_eq!(
        socket.bind_with_cred(
            SocketAddrEx::Netlink(NetlinkAddr {
                pid: bound.pid + 1,
                groups: group_bit(RTNLGRP_IPV4_MROUTE_R),
            }),
            &unprivileged,
        ),
        Err(LinuxError::EINVAL.into())
    );
    assert_eq!(
        socket.bind_with_cred(
            SocketAddrEx::Netlink(NetlinkAddr {
                pid: bound.pid,
                groups: group_bit(RTNLGRP_IPV4_MROUTE_R),
            }),
            &unprivileged,
        ),
        Err(LinuxError::EPERM.into())
    );

    // A failed group callback leaves the previous subscription installed.
    let SocketAddrEx::Netlink(unchanged) = socket.local_addr().unwrap() else {
        panic!("netlink local address");
    };
    assert_eq!(unchanged, bound);
}

#[def_test(serial)]
fn test_bind_group_lookup_and_protocol_group_masking() {
    let unprivileged = Cred::new(1000, 1000);

    // Only registered protocols have a group table.  A group request on an
    // unknown protocol therefore fails before the port-ID conformance check,
    // with ENOENT for a privileged caller and EPERM otherwise.
    let unknown = NetlinkSocket::new(999, NetlinkSocketType::Raw);
    assert_eq!(
        unknown.bind_with_cred(
            SocketAddrEx::Netlink(NetlinkAddr {
                pid: 608,
                groups: 1,
            }),
            &unprivileged,
        ),
        Err(LinuxError::EPERM.into())
    );
    assert_eq!(
        unknown.bind_with_cred(
            SocketAddrEx::Netlink(NetlinkAddr {
                pid: 608,
                groups: 1,
            }),
            &Cred::root(),
        ),
        Err(LinuxError::ENOENT.into())
    );
    assert_socket_unbound(&unknown);

    // Without a group request the group-table lookup is skipped, so an unknown
    // protocol can still allocate a plain port ID.
    unknown
        .bind_with_cred(
            SocketAddrEx::Netlink(NetlinkAddr {
                pid: 608,
                groups: 0,
            }),
            &Cred::root(),
        )
        .unwrap();

    // A protocol's group table masks unsupported bits instead of rejecting
    // them.  Linux raises the uevent group count to at least 32 when its
    // kernel socket is created, so all 32 `nl_groups` bits are subscribable.
    let uevent = NetlinkSocket::new(NETLINK_KOBJECT_UEVENT, NetlinkSocketType::Raw);
    uevent
        .bind_with_cred(
            SocketAddrEx::Netlink(NetlinkAddr {
                pid: 609,
                groups: u32::MAX,
            }),
            &unprivileged,
        )
        .unwrap();
    let SocketAddrEx::Netlink(bound) = uevent.local_addr().unwrap() else {
        panic!("netlink local address");
    };
    assert_eq!(bound.groups, u32::MAX);

    // Route implements all 32 group bits but gates the two reverse-path
    // groups on CAP_NET_ADMIN after the generic group-table masking.
    let route = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    assert_eq!(
        route.bind_with_cred(
            SocketAddrEx::Netlink(NetlinkAddr {
                pid: 610,
                groups: u32::MAX,
            }),
            &unprivileged,
        ),
        Err(LinuxError::EPERM.into())
    );
    assert_socket_unbound(&route);
    route
        .bind_with_cred(
            SocketAddrEx::Netlink(NetlinkAddr {
                pid: 610,
                groups: u32::MAX,
            }),
            &Cred::root(),
        )
        .unwrap();
    let SocketAddrEx::Netlink(bound) = route.local_addr().unwrap() else {
        panic!("netlink local address");
    };
    assert_eq!(bound.groups, u32::MAX);
}

#[def_test(serial)]
fn test_route_multicast_respects_bind_and_close_lifecycle() {
    let unbound = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    let subscriber = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    subscriber
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 231,
            groups: group_bit(RTNLGRP_LINK),
        }))
        .unwrap();

    publish_route_event(RTNLGRP_LINK, b"first link event");
    assert!(Pollable::poll(&subscriber).contains(IoEvents::IN));
    assert_eq!(
        subscriber.inner.rx_queue.lock().pop_front().unwrap().data,
        b"first link event"
    );
    assert!(!Pollable::poll(&subscriber).contains(IoEvents::IN));
    assert!(unbound.inner.rx_queue.lock().is_empty());

    // Repeating the port ID replaces the subscription mask, so the socket
    // must stop receiving link events immediately.
    subscriber
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 231,
            groups: group_bit(RTNLGRP_IPV4_IFADDR),
        }))
        .unwrap();
    publish_route_event(RTNLGRP_LINK, b"unsubscribed");
    assert!(subscriber.inner.rx_queue.lock().is_empty());

    // Dropping a subscriber invalidates its weak registry entry; a later
    // publication must neither resurrect nor block on the closed socket.
    drop(subscriber);
    publish_route_event(RTNLGRP_LINK, b"closed");

    // Linux raises the uevent group count to 32 even though kernel-originated
    // uevents are published to group 1, so bit 2 is a real subscription.
    let uevent = NetlinkSocket::new(NETLINK_KOBJECT_UEVENT, NetlinkSocketType::Raw);
    uevent
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 0, groups: 2 }))
        .unwrap();
    let SocketAddrEx::Netlink(uevent_bound) = uevent.local_addr().unwrap() else {
        panic!("netlink local address");
    };
    assert_eq!(uevent_bound.groups, 2);
    uevent
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: uevent_bound.pid,
            groups: 1,
        }))
        .unwrap();
}

#[def_test(serial)]
fn test_route_events_follow_mutations_and_subscriptions() {
    init_test_state();
    let subscriber = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    subscriber
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 201,
            groups: group_bit(RTNLGRP_LINK)
                | group_bit(RTNLGRP_IPV4_IFADDR)
                | group_bit(RTNLGRP_IPV4_ROUTE),
        }))
        .unwrap();
    let sender = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    sender
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 202,
            groups: 0,
        }))
        .unwrap();

    let address = Ipv4Address::new(192, 168, 1, 90);
    let request = build_ipv4_addr_mutation(
        address,
        72,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
    );
    sender
        .send_with_cred(
            Cursor::new(request.as_slice()),
            SendOptions::default(),
            &Cred::root(),
        )
        .unwrap();

    let packet = pop_event(&subscriber);
    // Address notifications carry the requesting port and sequence without
    // requiring `NLM_F_ECHO`.
    assert_eq!(event_header(&packet), (RTM_NEWADDR, 72, 0));
    assert_eq!(event_pid(&packet), 202);
    assert_eq!(packet.from.pid, 0);
    assert_eq!(packet.from.groups, group_bit(RTNLGRP_IPV4_IFADDR));
    let info = IfAddrMsg::read(&packet.data[NLMSG_HDR_LEN..]).unwrap();
    let (index, prefix_len) = (info.index, info.prefix_len);
    assert_eq!(index, 2);
    assert_eq!(prefix_len, 24);

    // The local /32 route is notified in the local table on the address's
    // device, matching what Linux's FIB installs rather than the internal
    // loopback-output lookup entry.
    let assert_derived_route = |packet: &NetlinkPacket, msg_type: u16| {
        // Linux's FIB notifier has no request context for connected/local
        // routes derived from an address change: its skb carries no sequence
        // and the FIB call passes a NULL portid.
        assert_eq!(event_header(packet), (msg_type, 0, 0));
        assert_eq!(event_pid(packet), 0);
        assert_eq!(packet.from.pid, 0);
        assert_eq!(packet.from.groups, group_bit(RTNLGRP_IPV4_ROUTE));
        let info = RtMsg::read(&packet.data[NLMSG_HDR_LEN..]).unwrap();
        let (dst_len, table, protocol, scope, route_type) = (
            info.dst_len,
            info.table,
            info.protocol,
            info.scope,
            info.route_type,
        );
        assert_eq!(dst_len, 32);
        assert_eq!(table, wire_route::TABLE_LOCAL);
        assert_eq!(protocol, wire_route::PROTOCOL_KERNEL);
        assert_eq!(scope, wire_route::SCOPE_HOST);
        assert_eq!(route_type, wire_route::TYPE_LOCAL);
        let attrs = parse_attrs(&packet.data[NLMSG_HDR_LEN + RtMsg::SIZE..]).unwrap();
        let attr = |kind| {
            attrs
                .iter()
                .find(|attr| attr.kind == kind)
                .map(|attr| attr.payload)
        };
        assert_eq!(
            attr(wire_route::attr::DST),
            Some([192, 168, 1, 90].as_slice())
        );
        assert_eq!(
            attr(wire_route::attr::OIF),
            Some(2i32.to_ne_bytes().as_slice())
        );
        // A secondary address's local /32 inherits the primary address as
        // its preferred source, even though its destination is the secondary.
        assert_eq!(
            attr(wire_route::attr::PREFSRC),
            Some([192, 168, 1, 2].as_slice())
        );
        assert_eq!(
            attr(wire_route::attr::TABLE),
            Some(u32::from(wire_route::TABLE_LOCAL).to_ne_bytes().as_slice())
        );
    };
    assert_derived_route(&pop_event(&subscriber), RTM_NEWROUTE);
    assert!(subscriber.inner.rx_queue.lock().is_empty());

    sender
        .send_with_cred(
            Cursor::new(build_link_update(2, 0, LINK_FLAG_UP).as_slice()),
            SendOptions::default(),
            &Cred::root(),
        )
        .unwrap();
    let packet = pop_event(&subscriber);
    // Link notifications stay 0/0 unless the request asked for an echo.
    assert_eq!(event_header(&packet), (RTM_NEWLINK, 0, 0));
    assert_eq!(event_pid(&packet), 0);
    assert_eq!(packet.from.pid, 0);
    assert_eq!(packet.from.groups, group_bit(RTNLGRP_LINK));
    let info = IfInfoMsg::read(&packet.data[NLMSG_HDR_LEN..]).unwrap();
    let (index, change) = (info.index, info.change);
    assert_eq!(index, 2);
    // The event reports every flag transition caused by the state change,
    // including device-owned flags such as LOWER_UP and RUNNING.
    assert_ne!(change & LINK_FLAG_UP, 0);

    // Bringing the link down makes the prefix and broadcast routes derived
    // from the address dead, and `fib_table_flush()` removes them from the
    // FIB.  Linux only sends RTM_DELROUTE from that flush when the preferred
    // source was removed, so the local /32 survives and no route deletion is
    // published.
    assert!(subscriber.inner.rx_queue.lock().is_empty());

    // Deleting the address removes the address first, then the route derived
    // from it, mirroring `__inet_del_ifa`.
    let mut request = build_ipv4_addr_mutation(address, 73, NLM_F_REQUEST | NLM_F_ACK);
    request[4..6].copy_from_slice(&RTM_DELADDR.to_ne_bytes());
    sender
        .send_with_cred(
            Cursor::new(request.as_slice()),
            SendOptions::default(),
            &Cred::root(),
        )
        .unwrap();
    let packet = pop_event(&subscriber);
    assert_eq!(event_header(&packet), (RTM_DELADDR, 73, 0));
    assert_eq!(event_pid(&packet), 202);
    assert_eq!(packet.from.groups, group_bit(RTNLGRP_IPV4_IFADDR));
    assert_derived_route(&pop_event(&subscriber), RTM_DELROUTE);
    assert!(subscriber.inner.rx_queue.lock().is_empty());

    // A no-op setlink emits no notification.
    sender
        .send_with_cred(
            Cursor::new(build_link_update(2, 0, LINK_FLAG_UP).as_slice()),
            SendOptions::default(),
            &Cred::root(),
        )
        .unwrap();
    assert!(subscriber.inner.rx_queue.lock().is_empty());
}

#[def_test(serial)]
fn test_ipv4_broadcast_control_publishes_address_and_route_events() {
    init_test_state();
    let subscriber = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    subscriber
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 211,
            groups: group_bit(RTNLGRP_IPV4_IFADDR) | group_bit(RTNLGRP_IPV4_ROUTE),
        }))
        .unwrap();

    let broadcast = Ipv4Address::new(192, 168, 1, 111);
    crate::set_interface_ipv4_broadcast("eth0", broadcast).unwrap();

    // `SIOCSIFBRDADDR` removes and re-inserts the address before the FIB
    // notifier installs the new explicit broadcast route.
    let removed = pop_event(&subscriber);
    assert_eq!(event_header(&removed), (RTM_DELADDR, 0, 0));
    assert_eq!(event_pid(&removed), 0);
    assert_eq!(removed.from.groups, group_bit(RTNLGRP_IPV4_IFADDR));

    let added = pop_event(&subscriber);
    assert_eq!(event_header(&added), (RTM_NEWADDR, 0, 0));
    assert_eq!(event_pid(&added), 0);
    assert_eq!(added.from.groups, group_bit(RTNLGRP_IPV4_IFADDR));
    let info = IfAddrMsg::read(&added.data[NLMSG_HDR_LEN..]).unwrap();
    assert_eq!((info.index, info.prefix_len), (2, 24));
    let attrs = parse_attrs(&added.data[NLMSG_HDR_LEN + IfAddrMsg::SIZE..]).unwrap();
    let broadcast_attr = attrs
        .iter()
        .find(|attr| attr.kind == wire_addr::attr::BROADCAST)
        .expect("IFA_BROADCAST");
    assert_eq!(broadcast_attr.payload, broadcast.octets());

    let route = pop_event(&subscriber);
    assert_eq!(event_header(&route), (RTM_NEWROUTE, 0, 0));
    assert_eq!(event_pid(&route), 0);
    assert_eq!(route.from.groups, group_bit(RTNLGRP_IPV4_ROUTE));
    let route_info = RtMsg::read(&route.data[NLMSG_HDR_LEN..]).unwrap();
    assert_eq!(route_info.dst_len, 32);
    assert_eq!(route_info.table, wire_route::TABLE_LOCAL);
    assert_eq!(route_info.protocol, wire_route::PROTOCOL_KERNEL);
    assert_eq!(route_info.scope, wire_route::SCOPE_LINK);
    assert_eq!(route_info.route_type, wire_route::TYPE_BROADCAST);
    let attrs = parse_attrs(&route.data[NLMSG_HDR_LEN + RtMsg::SIZE..]).unwrap();
    let attr = |kind| {
        attrs
            .iter()
            .find(|attr| attr.kind == kind)
            .map(|attr| attr.payload)
    };
    assert_eq!(
        attr(wire_route::attr::DST),
        Some(broadcast.octets().as_slice())
    );
    assert_eq!(
        attr(wire_route::attr::OIF),
        Some(2i32.to_ne_bytes().as_slice())
    );
    assert_eq!(
        attr(wire_route::attr::PREFSRC),
        Some([192, 168, 1, 2].as_slice())
    );
    assert_eq!(
        attr(wire_route::attr::TABLE),
        Some(u32::from(wire_route::TABLE_LOCAL).to_ne_bytes().as_slice())
    );
    assert!(subscriber.inner.rx_queue.lock().is_empty());

    // The stored broadcast already equals the requested value; `SIOCSIFBRDADDR`
    // therefore leaves the address untouched and publishes nothing.
    crate::set_interface_ipv4_broadcast("eth0", broadcast).unwrap();
    assert!(subscriber.inner.rx_queue.lock().is_empty());
}

#[def_test(serial)]
fn test_implicit_broadcast_assignment_replaces_the_address_without_route_change() {
    init_test_state();
    let subscriber = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    subscriber
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 212,
            groups: group_bit(RTNLGRP_IPV4_IFADDR) | group_bit(RTNLGRP_IPV4_ROUTE),
        }))
        .unwrap();

    // The test fixture models an address created without `IFA_BROADCAST`, so
    // its stored value is zero even though the CIDR has a computed broadcast.
    let computed = Ipv4Address::new(192, 168, 1, 255);
    crate::set_interface_ipv4_broadcast("eth0", computed).unwrap();

    let removed = pop_event(&subscriber);
    assert_eq!(event_header(&removed), (RTM_DELADDR, 0, 0));
    let added = pop_event(&subscriber);
    assert_eq!(event_header(&added), (RTM_NEWADDR, 0, 0));
    let attrs = parse_attrs(&added.data[NLMSG_HDR_LEN + IfAddrMsg::SIZE..]).unwrap();
    let broadcast = attrs
        .iter()
        .find(|attr| attr.kind == wire_addr::attr::BROADCAST)
        .expect("IFA_BROADCAST");
    assert_eq!(broadcast.payload, computed.octets());
    // The explicit value equals the computed network broadcast, so the two
    // Linux FIB additions collapse to the same project route.
    assert!(subscriber.inner.rx_queue.lock().is_empty());

    // A zero broadcast clears the stored value without changing that route.
    crate::set_interface_ipv4_broadcast("eth0", Ipv4Address::UNSPECIFIED).unwrap();
    let removed = pop_event(&subscriber);
    assert_eq!(event_header(&removed), (RTM_DELADDR, 0, 0));
    let added = pop_event(&subscriber);
    assert_eq!(event_header(&added), (RTM_NEWADDR, 0, 0));
    let attrs = parse_attrs(&added.data[NLMSG_HDR_LEN + IfAddrMsg::SIZE..]).unwrap();
    assert!(
        !attrs
            .iter()
            .any(|attr| attr.kind == wire_addr::attr::BROADCAST)
    );
    assert!(subscriber.inner.rx_queue.lock().is_empty());
}

#[def_test(serial)]
fn test_ioctl_address_assignment_stores_the_derived_broadcast() {
    init_test_state();
    let subscriber = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    subscriber
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 213,
            groups: group_bit(RTNLGRP_IPV4_IFADDR) | group_bit(RTNLGRP_IPV4_ROUTE),
        }))
        .unwrap();

    let address = core::net::Ipv4Addr::new(192, 168, 1, 99);
    crate::set_interface_ipv4_addr("eth0", address, 24).unwrap();

    let mut stored_broadcast = None;
    while !subscriber.inner.rx_queue.lock().is_empty() {
        let packet = pop_event(&subscriber);
        let header = NlMsgHeader::read(&packet.data).unwrap();
        if header.msg_type != RTM_NEWADDR {
            continue;
        }
        let attrs = parse_attrs(&packet.data[NLMSG_HDR_LEN + IfAddrMsg::SIZE..]).unwrap();
        let local = attrs
            .iter()
            .find(|attr| attr.kind == wire_addr::attr::ADDRESS)
            .map(|attr| attr.payload)
            .unwrap();
        if local != address.octets() {
            continue;
        }
        stored_broadcast = attrs
            .iter()
            .find(|attr| attr.kind == wire_addr::attr::BROADCAST)
            .map(|attr| attr.payload.to_vec());
    }
    assert_eq!(
        stored_broadcast,
        Some(Ipv4Address::new(192, 168, 1, 255).octets().to_vec())
    );

    // `SIOCSIFADDR` already stored the computed broadcast, so assigning the
    // same broadcast through `SIOCSIFBRDADDR` is a no-op.
    crate::set_interface_ipv4_broadcast("eth0", Ipv4Address::new(192, 168, 1, 255)).unwrap();
    assert!(subscriber.inner.rx_queue.lock().is_empty());
}

#[def_test(serial)]
fn test_netlink_connect_tracks_peer_and_filters_mismatched_unicast() {
    init_test_state();
    let receiver = NetlinkSocket::new(NETLINK_USERSOCK, NetlinkSocketType::Raw);
    receiver
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 631,
            groups: 0,
        }))
        .unwrap();
    let sender = NetlinkSocket::new(NETLINK_USERSOCK, NetlinkSocketType::Raw);
    sender
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 632,
            groups: 0,
        }))
        .unwrap();

    // `AF_UNSPEC` is a disconnect and succeeds even on an unconnected socket.
    assert!(
        sender
            .connect(SocketAddrEx::Unspecified, ConnectOptions::default())
            .is_ok()
    );

    sender
        .connect_with_cred(
            SocketAddrEx::Netlink(NetlinkAddr {
                pid: 631,
                groups: 0,
            }),
            &Cred::root(),
        )
        .unwrap();
    let SocketAddrEx::Netlink(peer) = sender.peer_addr().unwrap() else {
        panic!("netlink peer address");
    };
    assert_eq!(
        peer,
        NetlinkAddr {
            pid: 631,
            groups: 0
        }
    );

    // An address-less send uses the connected peer and source port.
    sender
        .send(Cursor::new(b"connected".as_slice()), SendOptions::default())
        .unwrap();
    let packet = pop_event(&receiver);
    assert_eq!(
        packet.from,
        NetlinkAddr {
            pid: 632,
            groups: 0
        }
    );
    assert_eq!(packet.data.as_slice(), b"connected");

    // A connected destination rejects unicast from anyone but its peer.
    receiver
        .connect_with_cred(
            SocketAddrEx::Netlink(NetlinkAddr {
                pid: 999,
                groups: 0,
            }),
            &Cred::root(),
        )
        .unwrap();
    let destination = SocketAddrEx::Netlink(NetlinkAddr {
        pid: 631,
        groups: 0,
    });
    let options = SendOptions {
        to: Some(destination.clone()),
        ..SendOptions::default()
    };
    assert_eq!(
        sender.send_with_cred(Cursor::new(b"rejected".as_slice()), options, &Cred::root()),
        Err(LinuxError::ECONNREFUSED.into())
    );
    assert_eq!(
        sender.send(
            Cursor::new(b"also rejected".as_slice()),
            SendOptions::default()
        ),
        Err(LinuxError::ECONNREFUSED.into())
    );
    assert!(receiver.inner.rx_queue.lock().is_empty());

    // Disconnecting removes the peer filter.
    receiver
        .connect_with_cred(SocketAddrEx::Unspecified, &Cred::root())
        .unwrap();
    sender
        .send(
            Cursor::new(b"after disconnect".as_slice()),
            SendOptions::default(),
        )
        .unwrap();
    assert_eq!(pop_event(&receiver).data.as_slice(), b"after disconnect");

    // Linux stores `ffs(nl_groups)`: only the lowest requested group is the
    // connected destination, and `getpeername` reports that normalised mask.
    sender
        .connect_with_cred(
            SocketAddrEx::Netlink(NetlinkAddr {
                pid: 631,
                groups: 0b1010,
            }),
            &Cred::root(),
        )
        .unwrap();
    let SocketAddrEx::Netlink(peer) = sender.peer_addr().unwrap() else {
        panic!("netlink peer address");
    };
    assert_eq!(peer.groups, 0b0010);
}

#[def_test(serial)]
fn test_netlink_connect_applies_protocol_authorization_and_autobinds() {
    init_test_state();
    let unprivileged = Cred::new(1000, 1000);

    // Route sockets do not allow unprivileged sends, so connecting to any
    // non-kernel destination requires CAP_NET_ADMIN.
    let route = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    let destination = SocketAddrEx::Netlink(NetlinkAddr {
        pid: 640,
        groups: 0,
    });
    assert_eq!(
        route.connect_with_cred(destination.clone(), &unprivileged),
        Err(LinuxError::EPERM.into())
    );
    assert_socket_unbound(&route);

    route.connect_with_cred(destination, &Cred::root()).unwrap();
    let SocketAddrEx::Netlink(local) = route.local_addr().unwrap() else {
        panic!("netlink local address");
    };
    assert_ne!(local.pid, 0);
    let SocketAddrEx::Netlink(peer) = route.peer_addr().unwrap() else {
        panic!("netlink peer address");
    };
    assert_eq!(peer.pid, 640);

    // Usersock explicitly permits unprivileged sends.
    let usersock = NetlinkSocket::new(NETLINK_USERSOCK, NetlinkSocketType::Raw);
    usersock
        .connect_with_cred(
            SocketAddrEx::Netlink(NetlinkAddr {
                pid: 641,
                groups: 0,
            }),
            &unprivileged,
        )
        .unwrap();
    let SocketAddrEx::Netlink(local) = usersock.local_addr().unwrap() else {
        panic!("netlink local address");
    };
    assert_ne!(local.pid, 0);
}

#[def_test(serial)]
fn test_netlink_recv_drops_datagram_when_userspace_copy_fails() {
    struct FailingDst;

    impl kio::Write for FailingDst {
        fn write(&mut self, _buf: &[u8]) -> kio::Result<usize> {
            Err(kerrno::KError::BadAddress)
        }

        fn flush(&mut self) -> kio::Result<()> {
            Ok(())
        }
    }

    impl kio::IoBufMut for FailingDst {
        fn remaining_mut(&self) -> usize {
            32
        }
    }

    let socket = NetlinkSocket::new(NETLINK_USERSOCK, NetlinkSocketType::Raw);
    socket.inner.rx_queue.lock().push_back(NetlinkPacket {
        from: NetlinkAddr {
            pid: 650,
            groups: 0,
        },
        data: vec![1, 2, 3, 4],
        credentials: None,
    });

    assert_eq!(
        socket.recv(FailingDst, RecvOptions::default()),
        Err(kerrno::KError::BadAddress)
    );
    // Linux dequeues before copying and frees the skb even after a copy error.
    assert!(socket.inner.rx_queue.lock().is_empty());

    // MSG_PEEK only borrows the skb, so a failed copy leaves it queued.
    socket.inner.rx_queue.lock().push_back(NetlinkPacket {
        from: NetlinkAddr {
            pid: 650,
            groups: 0,
        },
        data: vec![5, 6, 7, 8],
        credentials: None,
    });
    assert_eq!(
        socket.recv(
            FailingDst,
            RecvOptions {
                flags: RecvFlags::PEEK,
                ..RecvOptions::default()
            }
        ),
        Err(kerrno::KError::BadAddress)
    );
    assert_eq!(socket.inner.rx_queue.lock().packets.len(), 1);
}

#[def_test(serial)]
fn test_route_mutation_notifications_carry_wire_messages() {
    init_test_state();
    let route = RouteRequest {
        family: wire_route::FAMILY_IPV4,
        dst_len: 24,
        src_len: 0,
        table: RT_TABLE_MAIN,
        protocol: wire_route::PROTOCOL_BOOT,
        scope: wire_route::SCOPE_UNIVERSE,
        route_type: RTN_UNICAST,
        tos: 0,
        flags: 0,
        oif: 2,
        dst: Some(IpAddress::Ipv4(Ipv4Address::new(203, 0, 113, 0))),
        src: None,
        gateway: Some(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 3))),
        prefsrc: Some(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 2))),
    };
    let notification =
        notifications_for_events(add_route(route, NLM_F_CREATE | NLM_F_EXCL).unwrap())
            .into_iter()
            .next()
            .expect("new route notification");
    assert_eq!(notification.group, RTNLGRP_IPV4_ROUTE);
    let (msg_type, seq, _) = event_header(&NetlinkPacket {
        from: NetlinkAddr { pid: 0, groups: 0 },
        data: notification.data.clone(),
        credentials: None,
    });
    assert_eq!(msg_type, RTM_NEWROUTE);
    assert_eq!(seq, 0);
    let info = RtMsg::read(&notification.data[NLMSG_HDR_LEN..]).unwrap();
    assert_eq!(info.dst_len, 24);
    let attrs = parse_attrs(&notification.data[NLMSG_HDR_LEN + RtMsg::SIZE..]).unwrap();
    assert_eq!(
        attrs
            .iter()
            .find(|attr| attr.kind == wire_route::attr::TABLE)
            .expect("new route RTA_TABLE")
            .payload,
        u32::from(wire_route::TABLE_MAIN).to_ne_bytes()
    );

    let notification = notifications_for_events(del_route(route).unwrap())
        .into_iter()
        .next()
        .expect("deleted route notification");
    assert_eq!(notification.group, RTNLGRP_IPV4_ROUTE);
    assert_eq!(
        event_header(&NetlinkPacket {
            from: NetlinkAddr { pid: 0, groups: 0 },
            data: notification.data.clone(),
            credentials: None,
        })
        .0,
        RTM_DELROUTE
    );

    let neighbor = NeighRequest {
        family: wire_route::FAMILY_IPV4,
        ifindex: 2,
        state: wire_neigh::STATE_PERMANENT,
        flags: 0,
        dst: IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 12)),
        lladdr: Some([2, 3, 4, 5, 6, 7]),
    };
    let notification =
        notifications_for_events(add_neigh(neighbor, NLM_F_CREATE | NLM_F_EXCL).unwrap())
            .into_iter()
            .next()
            .expect("new neighbor notification");
    assert_eq!(notification.group, RTNLGRP_NEIGH);
    assert_eq!(
        event_header(&NetlinkPacket {
            from: NetlinkAddr { pid: 0, groups: 0 },
            data: notification.data.clone(),
            credentials: None,
        })
        .0,
        RTM_NEWNEIGH
    );
    let info = NdMsg::read(&notification.data[NLMSG_HDR_LEN..]).unwrap();
    let (ifindex, state) = (info.ifindex, info.state);
    assert_eq!(ifindex, 2);
    assert_eq!(state, wire_neigh::STATE_PERMANENT);
}

#[def_test(serial)]
fn test_user_multicast_uses_lowest_group_and_sender_port_id() {
    init_test_state();
    let link_subscriber = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    link_subscriber
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 211,
            groups: group_bit(RTNLGRP_LINK),
        }))
        .unwrap();
    let addr_subscriber = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    addr_subscriber
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 212,
            groups: group_bit(RTNLGRP_IPV4_IFADDR),
        }))
        .unwrap();
    let sender = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    sender
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 213,
            groups: group_bit(RTNLGRP_LINK),
        }))
        .unwrap();

    let query = build_nlmsg(
        RTM_GETLINK,
        73,
        NLM_F_REQUEST | NLM_F_DUMP,
        link_dump_payload(&[]),
    );
    let destination = NetlinkAddr {
        pid: 0,
        groups: group_bit(RTNLGRP_LINK) | group_bit(RTNLGRP_IPV4_IFADDR),
    };
    assert_eq!(
        sender.send_with_cred(
            Cursor::new(query.as_slice()),
            SendOptions {
                to: Some(SocketAddrEx::Netlink(destination)),
                ..SendOptions::default()
            },
            &Cred::root(),
        ),
        Ok(query.len())
    );

    // Only the lowest group is selected, and the sender is excluded from its
    // own broadcast.
    let packet = pop_event(&link_subscriber);
    assert_eq!(packet.from.pid, 213);
    assert_eq!(packet.from.groups, group_bit(RTNLGRP_LINK));
    assert_eq!(packet.data, query);
    assert!(addr_subscriber.inner.rx_queue.lock().is_empty());
    assert_eq!(event_header(&pop_event(&sender)).0, RTM_NEWLINK);

    // The request is still delivered to the kernel after the broadcast.
    assert_eq!(
        sender.send_with_cred(
            Cursor::new(query.as_slice()),
            SendOptions {
                to: Some(SocketAddrEx::Netlink(NetlinkAddr {
                    pid: 0,
                    groups: group_bit(RTNLGRP_IPV4_IFADDR),
                })),
                ..SendOptions::default()
            },
            &Cred::new(1000, 1000),
        ),
        Err(LinuxError::EPERM.into())
    );
    assert!(addr_subscriber.inner.rx_queue.lock().is_empty());
}

#[def_test(serial, user)]
fn test_autobind_starts_with_current_process_id() {
    let socket = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr::default()))
        .unwrap();

    let bound = socket.local_addr().unwrap().into_netlink().unwrap();
    assert_eq!(bound.pid, kprocess::current_user_process().visible_pid());
    assert_eq!(bound.groups, 0);
}

#[def_test(serial)]
fn test_unbound_route_socket_reports_zero_and_autobinds_on_send() {
    init_test_state();
    let first = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    let second = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    for socket in [&first, &second] {
        assert_eq!(
            socket.local_addr().unwrap().into_netlink().unwrap(),
            NetlinkAddr::default()
        );
    }

    let request = build_nlmsg(
        RTM_GETLINK,
        17,
        NLM_F_REQUEST | NLM_F_DUMP,
        link_dump_payload(&[]),
    );
    first
        .send_with_cred(
            Cursor::new(request.as_slice()),
            SendOptions::default(),
            &Cred::root(),
        )
        .unwrap();
    let first_addr = first.local_addr().unwrap().into_netlink().unwrap();
    assert_ne!(first_addr.pid, 0);
    assert_eq!(first_addr.groups, 0);

    second
        .send_with_cred(
            Cursor::new(request.as_slice()),
            SendOptions::default(),
            &Cred::root(),
        )
        .unwrap();
    let second_addr = second.local_addr().unwrap().into_netlink().unwrap();
    assert_ne!(second_addr.pid, 0);
    assert_ne!(second_addr.pid, first_addr.pid);
    assert_eq!(second_addr.groups, 0);

    // An explicit bind may not steal an autobound port ID.
    let contender = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    assert_eq!(
        contender.bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: first_addr.pid,
            groups: 0,
        })),
        Err(LinuxError::EADDRINUSE.into())
    );
}

#[def_test(serial)]
fn test_mutation_echo_follows_linux_notification_routing() {
    init_test_state();
    let subscriber = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    subscriber
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 220,
            groups: group_bit(RTNLGRP_LINK)
                | group_bit(RTNLGRP_IPV4_IFADDR)
                | group_bit(RTNLGRP_IPV4_ROUTE),
        }))
        .unwrap();
    let requester = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    requester
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 221,
            groups: 0,
        }))
        .unwrap();

    // Link transitions publish `RTM_NEWLINK` before the address/route
    // notifier work they trigger, so derived-route FIB notifications follow
    // the link event.  Those FIB notifications carry no request context.
    let consume_routes = |msg_type: u16, count: usize| {
        for _ in 0..count {
            let route = pop_event(&subscriber);
            assert_eq!(event_header(&route), (msg_type, 0, 0));
            assert_eq!(event_pid(&route), 0);
        }
    };

    // `NLM_F_ECHO` without `NLM_F_ACK` produces an echo but no ACK.  Address
    // notifications always carry the requester's sequence, and the echo is a
    // unicast copy in addition to the multicast that excludes that port.
    let address = Ipv4Address::new(192, 168, 1, 91);
    let request = build_ipv4_addr_mutation(
        address,
        92,
        NLM_F_REQUEST | NLM_F_CREATE | NLM_F_EXCL | NLM_F_ECHO,
    );
    requester
        .send_with_cred(
            Cursor::new(request.as_slice()),
            SendOptions::default(),
            &Cred::root(),
        )
        .unwrap();
    let multicast = pop_event(&subscriber);
    assert_eq!(event_header(&multicast), (RTM_NEWADDR, 92, 0));
    assert_eq!(event_pid(&multicast), 221);
    let echo = pop_event(&requester);
    assert_eq!(event_header(&echo), (RTM_NEWADDR, 92, 0));
    assert_eq!(event_pid(&echo), 221);
    assert_eq!(echo.from.pid, 0);
    // The address also installs its local /32 route.  The connected prefix and
    // broadcast routes already exist for the device's other address.  Linux
    // creates derived routes from the FIB notifier with a NULL request
    // context, so they are kernel-originated multicast notifications: port 0,
    // sequence 0, and no echo, even though the address request asked for
    // `NLM_F_ECHO`.
    consume_routes(RTM_NEWROUTE, 1);
    assert!(subscriber.inner.rx_queue.lock().is_empty());
    assert!(requester.inner.rx_queue.lock().is_empty());

    // Link notifications are 0/0 without an echo, and use the request's
    // sequence only when an echo was requested.
    requester
        .send_with_cred(
            Cursor::new(
                build_link_update_with(2, 0, LINK_FLAG_UP, 93, NLM_F_REQUEST | NLM_F_ECHO)
                    .as_slice(),
            ),
            SendOptions::default(),
            &Cred::root(),
        )
        .unwrap();
    let multicast = pop_event(&subscriber);
    assert_eq!(event_header(&multicast), (RTM_NEWLINK, 93, 0));
    assert_eq!(event_pid(&multicast), 221);
    let echo = pop_event(&requester);
    assert_eq!(event_header(&echo), (RTM_NEWLINK, 93, 0));
    assert_eq!(event_pid(&echo), 221);
    assert!(subscriber.inner.rx_queue.lock().is_empty());

    requester
        .send_with_cred(
            Cursor::new(
                build_link_update_with(2, LINK_FLAG_UP, LINK_FLAG_UP, 94, NLM_F_REQUEST).as_slice(),
            ),
            SendOptions::default(),
            &Cred::root(),
        )
        .unwrap();
    let multicast = pop_event(&subscriber);
    assert_eq!(event_header(&multicast), (RTM_NEWLINK, 0, 0));
    consume_routes(RTM_NEWROUTE, 2);
    assert_eq!(event_pid(&multicast), 0);
    assert!(requester.inner.rx_queue.lock().is_empty());

    // ACK and echo are independent: ACK alone does not echo, and echo alone
    // does not synthesize an ACK.
    requester
        .send_with_cred(
            Cursor::new(
                build_link_update_with(2, 0, LINK_FLAG_UP, 95, NLM_F_REQUEST | NLM_F_ACK)
                    .as_slice(),
            ),
            SendOptions::default(),
            &Cred::root(),
        )
        .unwrap();
    assert_eq!(event_header(&pop_event(&subscriber)), (RTM_NEWLINK, 0, 0));
    assert!(subscriber.inner.rx_queue.lock().is_empty());
    let ack = pop_event(&requester);
    assert_eq!(event_header(&ack).0, NLMSG_ERROR);
    assert_eq!(event_header(&ack).1, 95);
    assert!(requester.inner.rx_queue.lock().is_empty());

    requester
        .send_with_cred(
            Cursor::new(
                build_link_update_with(
                    2,
                    LINK_FLAG_UP,
                    LINK_FLAG_UP,
                    96,
                    NLM_F_REQUEST | NLM_F_ECHO,
                )
                .as_slice(),
            ),
            SendOptions::default(),
            &Cred::root(),
        )
        .unwrap();
    assert_eq!(event_header(&pop_event(&subscriber)), (RTM_NEWLINK, 96, 0));
    assert_eq!(event_header(&pop_event(&requester)), (RTM_NEWLINK, 96, 0));
    consume_routes(RTM_NEWROUTE, 2);
    assert!(requester.inner.rx_queue.lock().is_empty());

    // Deleting an address is request-originated for the address notification:
    // it carries the request sequence and port, and `NLM_F_ECHO` both excludes
    // the requester from multicast and queues one unicast copy.  The connected
    // and local routes removed as a consequence come from the FIB notifier
    // with no request context, so they remain 0/0 with sequence 0 and are not
    // echoed.
    let mut request = build_ipv4_addr_mutation(address, 97, NLM_F_REQUEST | NLM_F_ECHO);
    request[4..6].copy_from_slice(&RTM_DELADDR.to_ne_bytes());
    requester
        .send_with_cred(
            Cursor::new(request.as_slice()),
            SendOptions::default(),
            &Cred::root(),
        )
        .unwrap();
    let multicast = pop_event(&subscriber);
    assert_eq!(event_header(&multicast), (RTM_DELADDR, 97, 0));
    assert_eq!(event_pid(&multicast), 221);
    let echo = pop_event(&requester);
    assert_eq!(event_header(&echo), (RTM_DELADDR, 97, 0));
    assert_eq!(event_pid(&echo), 221);
    assert_eq!(echo.from.pid, 0);

    // The local /32 route derived from the address is removed after the
    // address notification.
    consume_routes(RTM_DELROUTE, 1);
    assert!(subscriber.inner.rx_queue.lock().is_empty());
    assert!(requester.inner.rx_queue.lock().is_empty());
}

#[def_test(serial)]
fn test_neighbor_control_notifications_do_not_echo() {
    init_test_state();
    let subscriber = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    subscriber
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 222,
            groups: group_bit(RTNLGRP_NEIGH),
        }))
        .unwrap();
    let requester = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    requester
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 223,
            groups: 0,
        }))
        .unwrap();

    let dst = Ipv4Address::new(192, 168, 1, 61);
    let lladdr = [2, 3, 4, 5, 6, 8];
    add_neigh(
        NeighRequest {
            family: wire_route::FAMILY_IPV4,
            ifindex: 2,
            state: wire_neigh::STATE_PERMANENT,
            flags: 0,
            dst: IpAddress::Ipv4(dst),
            lladdr: Some(lladdr),
        },
        NLM_F_CREATE | NLM_F_EXCL,
    )
    .unwrap();
    let _ = pop_event(&subscriber);

    let delete = build_neigh_delete(wire_route::FAMILY_IPV4, 2, &dst.octets(), 95);
    requester
        .send_with_cred(
            Cursor::new(delete.as_slice()),
            SendOptions::default(),
            &Cred::root(),
        )
        .unwrap();
    let notification = pop_event(&subscriber);
    assert_eq!(event_header(&notification), (RTM_DELNEIGH, 0, 0));
    assert_eq!(event_pid(&notification), 223);
    // Linux's neighbor notification path passes a NULL request header to
    // `rtnl_notify`, so NLM_F_ECHO cannot produce a unicast neighbor copy.
    let ack = pop_event(&requester);
    assert_eq!(event_header(&ack).0, NLMSG_ERROR);
    assert!(requester.inner.rx_queue.lock().is_empty());
}

#[def_test(serial)]
fn test_delneigh_notifies_subscribers_and_reports_lookup_failures() {
    init_test_state();
    let subscriber = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    subscriber
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 211,
            groups: group_bit(RTNLGRP_NEIGH),
        }))
        .unwrap();
    let sender = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    sender
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 212,
            groups: 0,
        }))
        .unwrap();

    let dst = Ipv4Address::new(192, 168, 1, 60);
    let lladdr = [2, 3, 4, 5, 6, 7];
    let request = NeighRequest {
        family: wire_route::FAMILY_IPV4,
        ifindex: 2,
        state: wire_neigh::STATE_PERMANENT,
        flags: 0,
        dst: IpAddress::Ipv4(dst),
        lladdr: Some(lladdr),
    };
    assert_eq!(
        add_neigh(request, NLM_F_CREATE | NLM_F_EXCL).unwrap().len(),
        1
    );
    let packet = pop_event(&subscriber);
    assert_eq!(event_header(&packet), (RTM_NEWNEIGH, 0, 0));
    let info = NdMsg::read(&packet.data[NLMSG_HDR_LEN..]).unwrap();
    let (family, ifindex, state) = (info.family, info.ifindex, info.state);
    assert_eq!(family, wire_route::FAMILY_IPV4);
    assert_eq!(ifindex, 2);
    assert_eq!(state, wire_neigh::STATE_PERMANENT);

    let delete = build_neigh_delete(wire_route::FAMILY_IPV4, 2, &dst.octets(), 61);
    sender
        .send_with_cred(
            Cursor::new(delete.as_slice()),
            SendOptions::default(),
            &Cred::root(),
        )
        .unwrap();
    assert_eq!(
        read_i32_ne(&pop_event(&sender).data, NLMSG_HDR_LEN),
        Some(0)
    );

    let packet = pop_event(&subscriber);
    // Neighbors report the requester PID in the payload header but no
    // sequence, and do not exclude the requester from the multicast.
    assert_eq!(event_header(&packet), (RTM_DELNEIGH, 0, 0));
    assert_eq!(event_pid(&packet), 212);
    assert_eq!(packet.from.pid, 0);
    assert_eq!(packet.from.groups, group_bit(RTNLGRP_NEIGH));
    let info = NdMsg::read(&packet.data[NLMSG_HDR_LEN..]).unwrap();
    let (family, ifindex) = (info.family, info.ifindex);
    assert_eq!(family, wire_route::FAMILY_IPV4);
    assert_eq!(ifindex, 2);
    let attrs = parse_attrs(&packet.data[NLMSG_HDR_LEN + 12..]).unwrap();
    assert_eq!(
        attrs
            .iter()
            .find(|attr| attr.kind == wire_neigh::attr::DST)
            .map(|attr| attr.payload),
        Some(dst.octets().as_slice())
    );
    assert_eq!(
        attrs
            .iter()
            .find(|attr| attr.kind == wire_neigh::attr::LLADDR)
            .map(|attr| attr.payload),
        Some(lladdr.as_slice())
    );
    assert!(!SERVICE.has_neighbor(
        NetDeviceId::new(2).unwrap(),
        crate::ip::IpAddress::Ipv4(crate::ip::Ipv4Address::from_octets(dst.octets()))
    ));

    // Deleting an absent entry reports ENOENT and publishes nothing.
    let delete = build_neigh_delete(wire_route::FAMILY_IPV4, 2, &dst.octets(), 62);
    sender
        .send_with_cred(
            Cursor::new(delete.as_slice()),
            SendOptions::default(),
            &Cred::root(),
        )
        .unwrap();
    assert_netlink_error(&sender, LinuxError::ENOENT);
    assert!(subscriber.inner.rx_queue.lock().is_empty());

    // A missing interface is ENODEV.
    let delete = build_neigh_delete(wire_route::FAMILY_IPV4, 99, &dst.octets(), 63);
    sender
        .send_with_cred(
            Cursor::new(delete.as_slice()),
            SendOptions::default(),
            &Cred::root(),
        )
        .unwrap();
    assert_netlink_error(&sender, LinuxError::ENODEV);
    assert!(subscriber.inner.rx_queue.lock().is_empty());

    // Only IPv4 neighbors are supported by the current stack.
    let delete = build_neigh_delete(10, 2, &[0; 16], 64);
    sender
        .send_with_cred(
            Cursor::new(delete.as_slice()),
            SendOptions::default(),
            &Cred::root(),
        )
        .unwrap();
    assert_netlink_error(&sender, LinuxError::EAFNOSUPPORT);
    assert!(subscriber.inner.rx_queue.lock().is_empty());
}

#[def_test(serial)]
fn test_device_unregister_publishes_teardown_events() {
    init_test_state();
    let neighbor_dst = IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 10));
    add_neigh(
        NeighRequest {
            family: wire_route::FAMILY_IPV4,
            ifindex: 2,
            state: wire_neigh::STATE_PERMANENT,
            flags: 0,
            dst: neighbor_dst,
            lladdr: Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]),
        },
        NLM_F_CREATE | NLM_F_EXCL,
    )
    .unwrap();
    let subscriber = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    subscriber
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 221,
            groups: group_bit(RTNLGRP_LINK)
                | group_bit(RTNLGRP_NEIGH)
                | group_bit(RTNLGRP_IPV4_IFADDR)
                | group_bit(RTNLGRP_IPV4_ROUTE),
        }))
        .unwrap();

    assert!(crate::unregister_netdev_for_test(kdevice::DeviceId::new(2)));

    let mut saw_down_link = false;
    let mut saw_address = false;
    let mut saw_route = false;
    let mut saw_neighbor = false;
    let mut last = None;
    let mut message_types = Vec::new();
    while let Some(packet) = subscriber.inner.rx_queue.lock().pop_front() {
        let (msg_type, seq, _) = event_header(&packet);
        message_types.push(msg_type);
        assert_eq!(seq, 0);
        assert_eq!(packet.from.pid, 0);
        match msg_type {
            RTM_NEWLINK => {
                assert!(!saw_down_link);
                assert!(message_types.len() == 1);
                assert_eq!(packet.from.groups, group_bit(RTNLGRP_LINK));
                let info = IfInfoMsg::read(&packet.data[NLMSG_HDR_LEN..]).unwrap();
                let index = info.index;
                let change = info.change;
                assert_eq!(index, 2);
                assert_eq!(change, LINK_FLAG_UP | LINK_FLAG_RUNNING);
                assert_eq!(info.flags & (LINK_FLAG_UP | LINK_FLAG_RUNNING), 0);
                let attrs = parse_attrs(&packet.data[NLMSG_HDR_LEN + IfInfoMsg::SIZE..]).unwrap();
                let operstate = attrs
                    .iter()
                    .find(|attr| attr.kind == wire_link::attr::OPERSTATE)
                    .map(|attr| attr.payload);
                assert_eq!(operstate, Some([IF_OPER_DOWN].as_slice()));
                saw_down_link = true;
            }
            RTM_DELADDR => {
                saw_address = true;
                assert_eq!(packet.from.groups, group_bit(RTNLGRP_IPV4_IFADDR));
                let info = IfAddrMsg::read(&packet.data[NLMSG_HDR_LEN..]).unwrap();
                let index = info.index;
                assert_eq!(index, 2);
            }
            RTM_DELROUTE => {
                saw_route = true;
                assert_eq!(packet.from.groups, group_bit(RTNLGRP_IPV4_ROUTE));
            }
            RTM_DELNEIGH => {
                saw_neighbor = true;
                assert_eq!(packet.from.groups, group_bit(RTNLGRP_NEIGH));
                let info = NdMsg::read(&packet.data[NLMSG_HDR_LEN..]).unwrap();
                let (ifindex, state, neigh_type) = (info.ifindex, info.state, info.neigh_type);
                assert_eq!(ifindex, 2);
                assert_eq!(state, wire_neigh::STATE_PERMANENT);
                assert_eq!(neigh_type, RTN_UNICAST);
                let attrs = parse_attrs(&packet.data[NLMSG_HDR_LEN + NdMsg::SIZE..]).unwrap();
                let dst = attrs
                    .iter()
                    .find(|attr| attr.kind == wire_neigh::attr::DST)
                    .map(|attr| attr.payload);
                assert_eq!(dst, Some([192, 168, 1, 10].as_slice()));
            }
            RTM_DELLINK => {
                assert!(last.is_none());
                assert_eq!(packet.from.groups, group_bit(RTNLGRP_LINK));
                let info = IfInfoMsg::read(&packet.data[NLMSG_HDR_LEN..]).unwrap();
                let (index, change) = (info.index, info.change);
                assert_eq!(index, 2);
                assert_eq!(change, u32::MAX);
                assert_eq!(info.flags & (LINK_FLAG_UP | LINK_FLAG_RUNNING), 0);
                last = Some(msg_type);
            }
            other => panic!("unexpected teardown message {other}"),
        }
    }
    assert!(saw_down_link);
    assert!(saw_address);
    assert!(saw_route);
    assert!(saw_neighbor);
    assert_eq!(last, Some(RTM_DELLINK));

    // Device teardown closes an active link first, removes each address before
    // the routes derived from it, then flushes the remaining routes, removes
    // neighbors, and finally emits RTM_DELLINK.  In particular, no route may
    // precede the first address and the neighbor must be gone before the final
    // link notification.
    assert_eq!(message_types.first(), Some(&RTM_NEWLINK));
    assert_eq!(message_types.get(1), Some(&RTM_DELADDR));
    let link_pos = message_types
        .iter()
        .position(|msg_type| *msg_type == RTM_DELLINK)
        .unwrap();
    let neigh_pos = message_types
        .iter()
        .position(|msg_type| *msg_type == RTM_DELNEIGH)
        .unwrap();
    assert_eq!(link_pos + 1, message_types.len());
    assert!(neigh_pos < link_pos);
    assert_eq!(
        message_types
            .iter()
            .filter(|msg_type| **msg_type == RTM_DELNEIGH)
            .count(),
        1
    );
}

#[def_test(serial)]
fn test_route_multicast_drops_when_subscriber_queue_is_full() {
    init_test_state();
    let subscriber = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    subscriber
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 231,
            groups: group_bit(RTNLGRP_LINK),
        }))
        .unwrap();
    assert!(subscriber.inner.rx_queue.lock().push_back(NetlinkPacket {
        from: NetlinkAddr { pid: 0, groups: 0 },
        data: vec![0; NETLINK_RX_QUEUE_LIMIT + 1],
        credentials: None,
    }));
    let sender = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    sender
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 232,
            groups: 0,
        }))
        .unwrap();
    sender
        .send_with_cred(
            Cursor::new(build_link_update(2, 0, LINK_FLAG_UP).as_slice()),
            SendOptions::default(),
            &Cred::root(),
        )
        .unwrap();
    assert_eq!(subscriber.inner.rx_queue.lock().packets.len(), 1);
}

#[def_test(serial)]
fn test_full_receive_queue_does_not_block_mutation_or_send() {
    init_test_state();
    let socket = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 46, groups: 0 }))
        .unwrap();
    assert!(socket.inner.rx_queue.lock().push_back(NetlinkPacket {
        from: NetlinkAddr { pid: 0, groups: 0 },
        data: vec![0; NETLINK_RX_QUEUE_LIMIT + 1],
        credentials: None,
    }));
    let address = Ipv4Address::new(192, 168, 1, 80);
    let request = build_ipv4_addr_mutation(
        address,
        55,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
    );

    // Linux enqueues the ACK with MSG_DONTWAIT after the callback has already
    // run.  A full receive queue drops the ACK; it neither fails the send nor
    // rolls back the address addition.
    assert_eq!(
        socket.send_with_cred(
            Cursor::new(request.as_slice()),
            SendOptions::default(),
            &Cred::root(),
        ),
        Ok(request.len())
    );
    assert!(SERVICE.ipv4_addr_entries().iter().any(|(dev, entry)| {
        entry.addr.address() == address.into() && *dev == NetDeviceId::new(2).unwrap()
    }));
    assert_eq!(socket.inner.rx_queue.lock().packets.len(), 1);
}
