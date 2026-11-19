// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::String, sync::Arc, vec};

use ksync::Mutex;
use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::{
    device::{LinkSendSnapshot, LinkSnapshot, LoopbackDevice},
    netfilter::{
        BaseMatch, Chain, FilterHook, FilterTable, InterfacePattern, Negated, Rule as FilterRule,
        Target, Verdict,
    },
};

type SentPackets = Arc<Mutex<Vec<(IpAddress, PacketBuf)>>>;

struct ForwardDevice {
    name: &'static str,
    mtu: usize,
    sent: SentPackets,
    link: LoopbackDevice,
}

impl NetDevice for ForwardDevice {
    fn name(&self) -> &str {
        self.name
    }

    fn link_kind(&self) -> LinkKind {
        LinkKind::Ethernet
    }

    fn mtu(&self) -> usize {
        self.mtu
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

    fn poll_rx(&mut self, _: i32, _: MonotonicInstant) -> KResult<RxPoll> {
        Ok(RxPoll::Empty)
    }

    fn has_rx_work(&self) -> bool {
        false
    }

    fn send_ip_packet(
        &mut self,
        _: i32,
        _: IpAddress,
        source: IpAddress,
        packet: PacketBuf,
        _: MonotonicInstant,
    ) -> bool {
        self.sent.lock().push((source, packet));
        false
    }

    fn register_rx_waker(
        &self,
        waker: &core::task::Waker,
        context: &mut PollContext<'_>,
    ) -> Result<(), PollRegisterError> {
        self.link.register_rx_waker(waker, context)
    }
}

fn router() -> (Router, SentPackets) {
    let mut router = Router::new();
    let sent = Arc::new(Mutex::new(Vec::new()));
    for (name, address, mtu) in [
        ("in0", Ipv4Address::new(192, 0, 2, 1), 1500),
        ("out0", Ipv4Address::new(203, 0, 113, 1), 68),
    ] {
        let dev = router
            .add_device(
                KBox::new(ForwardDevice {
                    name,
                    mtu,
                    sent: sent.clone(),
                    link: LoopbackDevice::new(),
                })
                .unwrap(),
            )
            .unwrap();
        router
            .add_ipv4_addr(
                dev,
                Ipv4AddrEntry {
                    addr: Ipv4Cidr::new(address, 24).unwrap(),
                    scope: ROUTE_SCOPE_UNIVERSE,
                    broadcast: None,
                    no_prefix_route: false,
                    label: None,
                },
            )
            .unwrap();
    }
    (router, sent)
}

fn filter(target: Target) -> FilterConfig {
    let base = BaseMatch {
        input_interface: Some(Negated::new(
            InterfacePattern::exact(String::from("in0")).unwrap(),
            false,
        )),
        output_interface: Some(Negated::new(
            InterfacePattern::exact(String::from("out0")).unwrap(),
            false,
        )),
        ..BaseMatch::default()
    };
    FilterConfig::new(
        FilterTable::new(vec![
            Chain::built_in(FilterHook::LocalIn, Verdict::Drop, vec![]),
            Chain::built_in(
                FilterHook::Forward,
                Verdict::Accept,
                vec![FilterRule::new(base, vec![], target).unwrap()],
            ),
            Chain::built_in(FilterHook::LocalOut, Verdict::Drop, vec![]),
        ])
        .unwrap(),
    )
}

fn packet(router: &Router, ttl: u8) -> PacketBuf {
    let bytes = ipv4::build_ipv4_packet(
        Ipv4Address::new(192, 0, 2, 7),
        Ipv4Address::new(203, 0, 113, 9),
        ipv4::PROTOCOL_UDP,
        ttl,
        &[0; 96],
    )
    .unwrap();
    PacketBuf::from_ip_packet_vec(
        router.devices.device_id_by_name("in0").unwrap().ifindex(),
        bytes,
        PacketOwner::DeviceRx,
    )
}

fn input(router: &Router, ttl: u8) -> Ipv4ForwardInput {
    let mut packet = packet(router, ttl);
    ipv4::validate_input_packet(&mut packet).unwrap();
    Ipv4ForwardInput::from_validated(packet).unwrap()
}

#[def_test]
fn forward_batch_counts_once_before_fragmentation() {
    let (mut router, sent) = router();
    let filter = filter(Target::Continue);
    let mut inputs = vec![input(&router, 64)];
    router.set_ipv4_forwarding(1);
    router.enqueue_forward_packets(&mut inputs);
    assert_eq!(
        router.dispatch_output_budgeted(MonotonicInstant::ORIGIN, 1, &filter),
        (1, true)
    );
    assert_eq!(sent.lock().len(), 1);
    assert_eq!(
        router.dispatch_output_budgeted(MonotonicInstant::ORIGIN, 1, &filter),
        (1, false)
    );
    let sent = sent.lock();
    assert_eq!(sent.len(), 2);
    for (index, (source, packet)) in sent.iter().enumerate() {
        assert_eq!(*source, IpAddress::Ipv4(Ipv4Address::new(203, 0, 113, 1)));
        let header = Ipv4Header::parse_input(packet.network_packet().unwrap()).unwrap();
        assert_eq!(header.bytes()[8], 63);
        assert_eq!(header.src_addr(), Ipv4Address::new(192, 0, 2, 7));
        assert_eq!(header.fragment_offset(), index * 48);
        assert_eq!(header.more_fragments(), index == 0);
        assert_eq!(
            packet.ifindex(),
            router.devices.device_id_by_name("out0").unwrap().ifindex()
        );
    }
    let counters = filter.with_state(|state| state.collect_counters());
    assert_eq!(counters[1].rules[0].packets, 1);
    assert_eq!(counters[1].rules[0].bytes, 116);
}

#[def_test]
fn forward_queue_backpressure_and_cleanup_preserve_local_tx_capacity() {
    let (mut router, sent) = router();
    let mut packets = vec![input(&router, 64)];
    router.enqueue_forward_packets(&mut packets);
    assert!(packets.is_empty());
    assert!(router.forward_queue.is_empty());
    router.set_ipv4_forwarding(1);
    let tx_capacity = router.available_tx_packet_slots();
    router.enqueue_forward_packets(
        &mut (0..SOCKET_BUFFER_SIZE)
            .map(|_| input(&router, 64))
            .collect(),
    );
    assert_eq!(router.ingress_capacity(), 0);
    assert_eq!(router.available_tx_packet_slots(), tx_capacity);
    assert_eq!(
        router.dispatch_budgeted(MonotonicInstant::ORIGIN, 1),
        (1, true)
    );
    assert_eq!(router.ingress_capacity(), 1);
    router.set_ipv4_forwarding(0);
    router.set_ipv4_forwarding(1);
    assert_eq!(
        router.dispatch_budgeted(MonotonicInstant::ORIGIN, 1),
        (0, false)
    );
    assert_eq!(sent.lock().len(), 1);
    router.enqueue_forward_packets(&mut vec![input(&router, 64)]);
    assert_eq!(
        router.dispatch_budgeted(MonotonicInstant::ORIGIN, 1),
        (1, true)
    );
    let output = router.devices.device_id_by_name("out0").unwrap();
    router.remove_ipv4_addr(
        output,
        Ipv4Cidr::new(Ipv4Address::new(203, 0, 113, 1), 24).unwrap(),
    );
    assert_eq!(
        router.dispatch_budgeted(MonotonicInstant::ORIGIN, 1),
        (0, false)
    );
    assert_eq!(sent.lock().len(), 2);
}

#[def_test]
fn forward_drop_does_not_submit_transmission_packets() {
    let (mut router, sent) = router();
    let filter = filter(Target::Verdict(Verdict::Drop));
    router.set_ipv4_forwarding(1);
    router.enqueue_forward_packets(&mut vec![input(&router, 64)]);
    assert_eq!(
        router.dispatch_output_budgeted(MonotonicInstant::ORIGIN, 1, &filter),
        (1, false)
    );
    assert!(sent.lock().is_empty());
}
