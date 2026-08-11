// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use alloc::{format, string::String, vec, vec::Vec};
use core::sync::atomic::Ordering;

use kcred::Cred;
use kerrno::LinuxError;
use kio::Cursor;
use smoltcp::wire::{IpAddress, IpCidr, Ipv4Address};
use unittest::def_test;

use super::{
    rtnetlink::*,
    socket::publish_kobject_uevent,
    wire::{
        NlMsgHeader, addr as wire_addr, build_nlmsg, link as wire_link, neigh as wire_neigh,
        push_attr, push_i32_ne, push_u8, push_u16_ne, push_u32_ne, read_i32_ne, read_u16_ne,
        route as wire_route,
    },
    *,
};
use crate::{RecvOptions, SendOptions, SocketAddrEx, socket::SocketOps};

fn init_test_state() {
    let state = build_initial_state(
        IpCidr::Ipv4(smoltcp::wire::Ipv4Cidr::new(
            Ipv4Address::new(127, 0, 0, 1),
            8,
        )),
        Some(IpCidr::Ipv4(smoltcp::wire::Ipv4Cidr::new(
            Ipv4Address::new(192, 168, 1, 2),
            24,
        ))),
        Some([0x02, 0x00, 0x00, 0x00, 0x00, 0x02]),
        Some(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 1))),
        1500,
    );
    if ROUTE_STATE.is_inited() {
        *ROUTE_STATE.write() = state;
    } else {
        init_route_state(state);
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
    let mut payload = Vec::new();
    push_u8(&mut payload, wire_route::FAMILY_IPV4);
    push_u8(&mut payload, 24);
    push_u8(&mut payload, 0);
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
fn test_build_initial_state_contains_links_and_routes() {
    let state = build_initial_state(
        IpCidr::Ipv4(smoltcp::wire::Ipv4Cidr::new(
            Ipv4Address::new(127, 0, 0, 1),
            8,
        )),
        Some(IpCidr::Ipv4(smoltcp::wire::Ipv4Cidr::new(
            Ipv4Address::new(10, 0, 2, 15),
            24,
        ))),
        Some([1, 2, 3, 4, 5, 6]),
        Some(IpAddress::Ipv4(Ipv4Address::new(10, 0, 2, 2))),
        1500,
    );

    assert_eq!(state.links.len(), 2);
    assert_eq!(state.addrs.len(), 2);
    assert_eq!(state.links[0].name, "lo");
    assert_eq!(state.links[1].name, "eth0");
    assert!(state.routes.iter().any(|route| {
        route.dst == Some(IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1)))
            && route.dst_len == 8
            && route.scope == wire_route::SCOPE_HOST
            && route.gateway.is_none()
    }));
    assert!(state.routes.iter().any(|route| {
        route.dst == Some(IpAddress::Ipv4(Ipv4Address::new(10, 0, 2, 15)))
            && route.dst_len == 32
            && route.scope == wire_route::SCOPE_HOST
            && route.gateway.is_none()
    }));
    assert!(state.routes.iter().any(|route| {
        route.dst.is_none()
            && route.dst_len == 0
            && route.scope == wire_route::SCOPE_UNIVERSE
            && route.gateway == Some(IpAddress::Ipv4(Ipv4Address::new(10, 0, 2, 2)))
    }));
}

#[def_test(serial)]
fn test_parse_link_update_name_and_mtu() {
    let mut payload = Vec::new();
    push_u8(&mut payload, 17);
    push_u8(&mut payload, 0);
    push_u16_ne(&mut payload, ARPHRD_ETHER);
    push_i32_ne(&mut payload, 2);
    push_u32_ne(&mut payload, IFF_UP);
    push_u32_ne(&mut payload, IFF_UP);
    payload.extend(attr(wire_link::attr::IFNAME, b"eth1\0"));
    payload.extend(attr(wire_link::attr::MTU, &1400u32.to_ne_bytes()));

    let request = parse_link_update(&payload).unwrap();
    assert_eq!(request.index, 2);
    assert_eq!(request.name, Some(String::from("eth1")));
    assert_eq!(request.mtu, Some(1400));
}

#[def_test(serial)]
fn test_update_link_state_rename_and_down() {
    init_test_state();
    let request = LinkUpdateRequest {
        index: 2,
        flags: 0,
        change: IFF_UP,
        name: Some(String::from("ens3")),
        mtu: Some(1400),
    };
    update_link_state(request).unwrap();

    let state = route_state();
    let link = state.links.iter().find(|link| link.index == 2).unwrap();
    assert_eq!(link.name, "ens3");
    assert_eq!(link.mtu, 1400);
    assert_eq!(link.flags & IFF_UP, 0);
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

    add_addr_state(req, NLM_F_CREATE | NLM_F_EXCL).unwrap();
    assert!(
        route_state()
            .addrs
            .iter()
            .any(|addr| addr.address == req.address && addr.index == 2)
    );

    let replaced = AddrRequest {
        scope: wire_route::SCOPE_HOST,
        ..req
    };
    add_addr_state(replaced, NLM_F_REPLACE).unwrap();
    assert!(
        route_state()
            .addrs
            .iter()
            .any(|addr| addr.address == replaced.address && addr.scope == wire_route::SCOPE_HOST)
    );

    del_addr_state(replaced).unwrap();
    assert!(
        !route_state()
            .addrs
            .iter()
            .any(|addr| addr.address == replaced.address && addr.index == 2)
    );
}

#[def_test(serial)]
fn test_add_and_delete_route() {
    init_test_state();
    let req = RouteRequest {
        family: wire_route::FAMILY_IPV4,
        dst_len: 24,
        table: RT_TABLE_MAIN,
        protocol: wire_route::PROTOCOL_BOOT,
        scope: wire_route::SCOPE_UNIVERSE,
        route_type: RTN_UNICAST,
        oif: 2,
        dst: Some(IpAddress::Ipv4(Ipv4Address::new(172, 16, 0, 0))),
        gateway: Some(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 1))),
        prefsrc: Some(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 2))),
    };

    add_route_state(req, NLM_F_CREATE | NLM_F_EXCL).unwrap();
    assert!(
        route_state()
            .routes
            .iter()
            .any(|route| route.dst == req.dst && route.gateway == req.gateway)
    );

    del_route_state(req).unwrap();
    assert!(
        !route_state()
            .routes
            .iter()
            .any(|route| route.dst == req.dst && route.gateway == req.gateway)
    );
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

    add_neigh_state(req, NLM_F_CREATE | NLM_F_REPLACE).unwrap();
    assert!(
        route_state()
            .neighs
            .iter()
            .any(|neigh| neigh.dst == req.dst && neigh.lladdr == req.lladdr)
    );

    let replaced = NeighRequest {
        lladdr: Some([1, 2, 3, 4, 5, 6]),
        ..req
    };
    add_neigh_state(replaced, NLM_F_REPLACE).unwrap();
    assert!(
        route_state()
            .neighs
            .iter()
            .any(|neigh| neigh.dst == replaced.dst && neigh.lladdr == replaced.lladdr)
    );
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

    let state = route_state();
    assert!(
        state
            .addrs
            .iter()
            .any(|addr| { addr.address == IpAddress::Ipv4(first_addr) && addr.index == 2 })
    );
    assert!(
        state
            .addrs
            .iter()
            .any(|addr| { addr.address == IpAddress::Ipv4(second_addr) && addr.index == 2 })
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
        !route_state()
            .addrs
            .iter()
            .any(|addr| { addr.address == IpAddress::Ipv4(address) && addr.index == 2 })
    );
}

#[def_test(serial)]
fn test_malformed_nlmsg_len_does_not_mutate_route_state() {
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
        !route_state()
            .addrs
            .iter()
            .any(|addr| { addr.address == IpAddress::Ipv4(address) && addr.index == 2 })
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
fn test_mutation_queue_reservation_failure_preserves_route_state() {
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
        !route_state()
            .addrs
            .iter()
            .any(|addr| { addr.address == IpAddress::Ipv4(address) && addr.index == 2 })
    );
}
