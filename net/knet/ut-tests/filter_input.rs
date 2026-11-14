// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::ToString, vec, vec::Vec};
use core::net::{Ipv4Addr, SocketAddrV4};

use ktime_types::{MonotonicInstant, TimeSpan};
use unittest::def_test;

use super::{
    chain::{Chain, FilterHook},
    config::FilterConfig,
    matcher::{BaseMatch, InterfacePattern, Negated},
    rule::{Rule, Target, Verdict},
    table::FilterTable,
};
use crate::{
    SocketAddrEx, SocketOps,
    buf::{PacketBuf, PacketOwner},
    device::net_rx::{self, RxPacket},
    ip::{Ipv4Address, Ipv4Cidr},
    ipv4,
    stack::ingress::IngressProcessor,
    udp::UdpSocket,
};

const IFINDEX: i32 = 1_000_007;

fn table(policy: Verdict, rules: Vec<Rule>) -> FilterTable {
    FilterTable::new(vec![
        Chain::built_in(FilterHook::LocalIn, policy, rules),
        Chain::built_in(FilterHook::Forward, Verdict::Accept, vec![]),
        Chain::built_in(FilterHook::LocalOut, Verdict::Accept, vec![]),
    ])
    .unwrap()
}

fn datagram(protocol: u8, payload: &[u8]) -> PacketBuf {
    let bytes = ipv4::build_ipv4_packet(
        Ipv4Address::LOOPBACK,
        Ipv4Address::LOOPBACK,
        protocol,
        64,
        payload,
    )
    .unwrap();
    PacketBuf::from_ip_packet_vec(IFINDEX, bytes, PacketOwner::DeviceRx)
}

fn udp_packet(port: u16) -> PacketBuf {
    let mut payload = vec![0; 9];
    payload[..2].copy_from_slice(&12345u16.to_be_bytes());
    payload[2..4].copy_from_slice(&port.to_be_bytes());
    payload[4..6].copy_from_slice(&9u16.to_be_bytes());
    payload[8] = 42;
    datagram(ipv4::PROTOCOL_UDP, &payload)
}

fn ingress() -> IngressProcessor {
    let mut ingress = IngressProcessor::new();
    ingress.update_local_ipv4_addrs(&[Ipv4Cidr::new(Ipv4Address::LOOPBACK, 8).unwrap()]);
    ingress.update_device_names([(IFINDEX, "lo")].into_iter());
    ingress
}

fn receive(
    ingress: &mut IngressProcessor,
    filter: &FilterConfig,
    packet: impl Into<RxPacket>,
) -> (Vec<PacketBuf>, Vec<Vec<u8>>) {
    let mut accepted = vec![];
    let mut control = vec![];
    ingress.handle_rx_packets(
        MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(1)),
        filter,
        &mut vec![packet.into()],
        &mut accepted,
        &mut control,
    );
    (accepted, control)
}

fn input_packets(filter: &FilterConfig) -> u64 {
    filter.with_state(|state| state.collect_counters()[0].end.packets)
}

#[def_test]
fn input_drop_precedes_transport_delivery_and_control_replies() {
    for protocol in [ipv4::PROTOCOL_TCP, ipv4::PROTOCOL_ICMP, 253] {
        for policy in [Verdict::Accept, Verdict::Drop] {
            let filter = FilterConfig::new(table(policy, vec![]));
            let packet = datagram(protocol, &[0; 24]);
            let size = packet.network_packet().unwrap().len() as u64;
            let (accepted, control) = receive(&mut ingress(), &filter, packet);
            assert_eq!(
                accepted.len(),
                usize::from(policy == Verdict::Accept && protocol != 253)
            );
            assert_eq!(
                control.len(),
                usize::from(policy == Verdict::Accept && protocol == 253)
            );
            filter.with_state(|state| {
                let counters = state.collect_counters();
                assert_eq!(counters[0].end.packets, 1);
                assert_eq!(counters[0].end.bytes, size);
                assert_eq!(counters[1].end.packets, 0);
                assert_eq!(counters[2].end.packets, 0);
            });
        }
    }
    let filter = FilterConfig::new(table(Verdict::Drop, vec![]));
    let (accepted, control) = receive(&mut ingress(), &filter, udp_packet(64231));
    assert!(accepted.is_empty() && control.is_empty());
    assert_eq!(input_packets(&filter), 1);
}

#[def_test]
fn input_uses_the_current_interface_name_and_no_output_interface() {
    let rule = Rule::new(
        BaseMatch {
            input_interface: Some(Negated::new(
                InterfacePattern::exact("renamed".to_string()).unwrap(),
                false,
            )),
            output_interface: Some(Negated::new(
                InterfacePattern::exact("".to_string()).unwrap(),
                false,
            )),
            ..Default::default()
        },
        vec![],
        Target::Verdict(Verdict::Accept),
    )
    .unwrap();
    let filter = FilterConfig::new(table(Verdict::Drop, vec![rule]));
    let mut ingress = ingress();
    assert!(
        receive(
            &mut ingress,
            &filter,
            datagram(ipv4::PROTOCOL_TCP, &[0; 20])
        )
        .0
        .is_empty()
    );
    ingress.update_device_names([(IFINDEX, "renamed")].into_iter());
    assert_eq!(
        receive(
            &mut ingress,
            &filter,
            datagram(ipv4::PROTOCOL_TCP, &[0; 20])
        )
        .0
        .len(),
        1
    );
    ingress.update_device_names(core::iter::empty());
    assert!(
        receive(
            &mut ingress,
            &filter,
            datagram(ipv4::PROTOCOL_TCP, &[0; 20])
        )
        .0
        .is_empty()
    );
    filter.with_state(|state| assert_eq!(state.collect_counters()[0].rules[0].packets, 1));
    assert_eq!(input_packets(&filter), 1);
}

#[def_test]
fn input_counts_a_complete_reassembled_datagram_once() {
    let filter = FilterConfig::new(table(Verdict::Drop, vec![]));
    let packet = datagram(ipv4::PROTOCOL_TCP, &[0; 40]);
    let fragments = ipv4::fragment_output_packet(packet.network_packet().unwrap(), 36, 17).unwrap();
    assert!(fragments.len() > 1);
    let mut ingress = ingress();
    for (index, bytes) in fragments.iter().rev().enumerate() {
        let packet = PacketBuf::from_ip_packet_vec(IFINDEX, bytes.clone(), PacketOwner::DeviceRx);
        let (accepted, control) = receive(&mut ingress, &filter, packet);
        assert!(accepted.is_empty() && control.is_empty());
        assert_eq!(
            input_packets(&filter),
            u64::from(index + 1 == fragments.len())
        );
    }
    filter.with_state(|state| assert_eq!(state.collect_counters()[0].end.bytes, 60));
}

#[def_test(serial)]
fn loopback_udp_input_drop_and_accept_reach_the_receive_queue() {
    let socket = UdpSocket::new();
    socket
        .bind(SocketAddrEx::Ip(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 64232).into(),
        ))
        .unwrap();
    let filter = FilterConfig::new(table(Verdict::Drop, vec![]));
    let queued = net_rx::queued_len();
    net_rx::enqueue(udp_packet(64232), "lo", &filter).unwrap();
    assert_eq!(net_rx::queued_len(), queued);
    assert!(socket.recv_datagram_now(&mut [0; 1]).unwrap().is_none());
    filter
        .replace(table(Verdict::Accept, vec![]), |_, _| Ok(()))
        .unwrap();
    net_rx::enqueue(udp_packet(64232), "lo", &filter).unwrap();
    net_rx::process_pending(net_rx::NET_RX_BUDGET);
    let mut data = [0];
    assert_eq!(socket.recv_datagram_now(&mut data).unwrap().unwrap().0, 1);
    assert_eq!(data, [42]);
    assert_eq!(input_packets(&filter), 1);
    assert!(net_rx::pop_deferred(IFINDEX).is_none());
}

#[def_test(serial)]
fn unmatched_loopback_udp_keeps_its_input_result_across_replacement() {
    let filter = FilterConfig::new(table(Verdict::Accept, vec![]));
    net_rx::enqueue(udp_packet(64233), "lo", &filter).unwrap();
    net_rx::process_pending(net_rx::NET_RX_BUDGET);
    let packet = net_rx::pop_deferred(IFINDEX).unwrap();
    assert!(matches!(&packet, RxPacket::LocalUdp(_)));
    let (old, _) = filter
        .replace(table(Verdict::Drop, vec![]), |_, _| Ok(()))
        .unwrap();
    let (accepted, control) = receive(&mut ingress(), &filter, packet);
    assert!(accepted.is_empty());
    assert_eq!(control.len(), 1);
    assert_eq!(old.collect_counters()[0].end.packets, 1);
    assert_eq!(input_packets(&filter), 0);
}

#[def_test(serial)]
fn full_loopback_queue_rejects_udp_before_input() {
    let filter = FilterConfig::new(table(Verdict::Accept, vec![]));
    while net_rx::enqueue(datagram(ipv4::PROTOCOL_TCP, &[0; 20]), "lo", &filter).is_ok() {}
    let rejected = net_rx::enqueue(udp_packet(64235), "lo", &filter);
    let counted = input_packets(&filter);
    net_rx::discard_ifindex(IFINDEX);
    assert!(rejected.is_err());
    assert_eq!(counted, 0);
}
