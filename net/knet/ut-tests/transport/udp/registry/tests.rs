// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec};

use ::core::net::{SocketAddr, SocketAddrV4};
use unittest::def_test;

use super::*;
use crate::ip::Ipv4Address;

fn endpoint(addr: Ipv4Address, port: u16) -> IpEndpoint {
    SocketAddrV4::new(ipv4_to_core(addr), port).into()
}

fn socket_addr(addr: Ipv4Address, port: u16) -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(ipv4_to_core(addr), port))
}

fn clear_registry() {
    clear_udp_registry_for_test();
}

fn bind_test_pcb(local: IpEndpoint, reuse_address: bool) -> Arc<UdpPcb> {
    let pcb = UdpPcb::new(UdpSocketState::new());
    bind_udp_pcb(pcb.clone(), local, reuse_address).unwrap();
    pcb
}

#[def_test]
fn bucket_index_uses_high_port_bits() {
    let first = 40000;
    let second = first + UDP_REGISTRY_BUCKETS as u16;

    assert_ne!(bucket_index_for_port(first), bucket_index_for_port(second));
}

#[def_test]
fn ephemeral_port_iter_visits_every_candidate_once() {
    let mut seen = vec![false; UDP_EPHEMERAL_PORT_RANGE as usize];
    let mut count = 0;

    for port in UdpEphemeralPortIter::new(0x1234_5679) {
        assert!((UDP_EPHEMERAL_PORT_START..=UDP_EPHEMERAL_PORT_END).contains(&port));
        let index = (port - UDP_EPHEMERAL_PORT_START) as usize;
        assert!(!seen[index]);
        seen[index] = true;
        count += 1;
    }

    assert_eq!(count, UDP_EPHEMERAL_PORT_RANGE as usize);
}

#[def_test(serial)]
fn bind_registry_rejects_wildcard_conflict() {
    clear_registry();

    bind_test_pcb(endpoint(Ipv4Address::UNSPECIFIED, 8080), false);
    assert!(
        bind_udp_pcb(
            UdpPcb::new(UdpSocketState::new()),
            endpoint(Ipv4Address::new(10, 0, 0, 2), 8080),
            false
        )
        .is_err()
    );

    clear_registry();
}

#[def_test(serial)]
fn reuse_bind_is_recorded_for_later_nonreuse_conflict() {
    clear_registry();

    bind_test_pcb(endpoint(Ipv4Address::new(10, 0, 0, 2), 8081), true);
    assert!(
        bind_udp_pcb(
            UdpPcb::new(UdpSocketState::new()),
            endpoint(Ipv4Address::new(10, 0, 0, 2), 8081),
            true
        )
        .is_ok()
    );
    assert!(
        bind_udp_pcb(
            UdpPcb::new(UdpSocketState::new()),
            endpoint(Ipv4Address::new(10, 0, 0, 2), 8081),
            false
        )
        .is_err()
    );

    clear_registry();
}

#[def_test(serial)]
fn reuse_bind_rejects_reuse_after_nonreuse() {
    clear_registry();

    bind_test_pcb(endpoint(Ipv4Address::new(10, 0, 0, 2), 8082), false);
    assert!(
        bind_udp_pcb(
            UdpPcb::new(UdpSocketState::new()),
            endpoint(Ipv4Address::new(10, 0, 0, 2), 8082),
            true
        )
        .is_err()
    );

    clear_registry();
}

#[def_test(serial)]
fn ephemeral_bind_assigns_distinct_registered_ports() {
    clear_registry();

    let first = bind_udp_auto_ephemeral_pcb(
        UdpPcb::new(UdpSocketState::new()),
        IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
        false,
    )
    .unwrap();
    let second = bind_udp_auto_ephemeral_pcb(
        UdpPcb::new(UdpSocketState::new()),
        IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
        false,
    )
    .unwrap();

    assert_ne!(first.port, second.port);

    clear_registry();
}

#[def_test(serial)]
fn connected_pcb_takes_lookup_precedence() {
    clear_registry();

    let local = endpoint(Ipv4Address::new(10, 0, 0, 2), 8080);
    let remote = endpoint(Ipv4Address::new(192, 0, 2, 1), 5353);

    let bound_state = UdpSocketState::new();
    bound_state.set_local_endpoint(Some(local));
    let bound = UdpPcb::new(bound_state);
    register_udp_pcb(bound);

    let connected_state = UdpSocketState::new();
    connected_state.set_local_endpoint(Some(local));
    connected_state.set_peer_endpoint(Some((remote, local.addr)));
    let connected = UdpPcb::new(connected_state);
    register_udp_pcb(connected.clone());

    let selected = lookup_udp_pcb(
        socket_addr(Ipv4Address::new(10, 0, 0, 2), 8080),
        socket_addr(Ipv4Address::new(192, 0, 2, 1), 5353),
    )
    .expect("connected PCB should match");

    assert!(Arc::ptr_eq(&selected, &connected));
    clear_registry();
}

#[def_test(serial)]
fn lookup_scans_only_matching_local_port_bucket() {
    clear_registry();

    let first_state = UdpSocketState::new();
    first_state.set_local_endpoint(Some(endpoint(Ipv4Address::new(10, 0, 0, 2), 8080)));
    let first = UdpPcb::new(first_state);
    register_udp_pcb(first);

    let second_state = UdpSocketState::new();
    second_state.set_local_endpoint(Some(endpoint(Ipv4Address::new(10, 0, 0, 2), 5353)));
    let second = UdpPcb::new(second_state);
    register_udp_pcb(second.clone());

    let selected = lookup_udp_pcb(
        socket_addr(Ipv4Address::new(10, 0, 0, 2), 5353),
        socket_addr(Ipv4Address::new(192, 0, 2, 1), 49152),
    )
    .expect("same-port UDP PCB should match");

    assert!(Arc::ptr_eq(&selected, &second));
    clear_registry();
}

#[def_test(serial)]
fn lookup_scores_specific_and_wildcard_addrs() {
    clear_registry();

    let local = endpoint(Ipv4Address::new(10, 0, 0, 2), 8083);
    let wildcard = endpoint(Ipv4Address::UNSPECIFIED, 8083);
    let remote = endpoint(Ipv4Address::new(192, 0, 2, 1), 5353);

    let specific_state = UdpSocketState::new();
    specific_state.set_local_endpoint(Some(local));
    let specific = UdpPcb::new(specific_state);
    register_udp_pcb(specific);

    let wildcard_state = UdpSocketState::new();
    wildcard_state.set_local_endpoint(Some(wildcard));
    wildcard_state.set_peer_endpoint(Some((remote, wildcard.addr)));
    let wildcard_connected = UdpPcb::new(wildcard_state);
    register_udp_pcb(wildcard_connected.clone());

    let selected = lookup_udp_pcb(
        socket_addr(Ipv4Address::new(10, 0, 0, 2), 8083),
        socket_addr(Ipv4Address::new(192, 0, 2, 1), 5353),
    )
    .expect("wildcard connected PCB should match");

    assert!(Arc::ptr_eq(&selected, &wildcard_connected));
    clear_registry();
}

#[def_test(serial)]
fn lookup_skips_pcb_bound_to_another_device() {
    clear_registry();

    let local = endpoint(Ipv4Address::new(10, 0, 0, 2), 8068);
    let bound_state = UdpSocketState::new();
    bound_state.set_local_endpoint(Some(local));
    bound_state.set_bound_dev_if_for_test(2);
    let bound = UdpPcb::new(bound_state);
    register_udp_pcb(bound.clone());

    assert!(
        lookup_udp_pcb_on_device(
            socket_addr(Ipv4Address::new(10, 0, 0, 2), 8068),
            socket_addr(Ipv4Address::new(192, 0, 2, 1), 67),
            1,
        )
        .is_none()
    );
    let selected = lookup_udp_pcb_on_device(
        socket_addr(Ipv4Address::new(10, 0, 0, 2), 8068),
        socket_addr(Ipv4Address::new(192, 0, 2, 1), 67),
        2,
    )
    .expect("bound PCB should match its device");
    assert!(Arc::ptr_eq(&selected, &bound));
    clear_registry();
}

#[def_test(serial)]
fn unregister_removes_pcb_and_releases_port() {
    clear_registry();

    let local = endpoint(Ipv4Address::new(10, 0, 0, 2), 8084);
    let pcb = bind_test_pcb(local, false);

    unregister_udp_pcb(&pcb);

    assert!(
        lookup_udp_pcb(
            socket_addr(Ipv4Address::new(10, 0, 0, 2), 8084),
            socket_addr(Ipv4Address::new(192, 0, 2, 1), 49152)
        )
        .is_none()
    );
    assert!(udp_port_available(listen_endpoint(local)));
    clear_registry();
}
