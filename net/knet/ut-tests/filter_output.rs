// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::ToString, sync::Arc, vec, vec::Vec};

use kalloc_types::KBox;
use kerrno::KResult;
use kpoll::{PollContext, PollRegisterError};
use ksync::Mutex;
use ktime_types::MonotonicInstant;
use unittest::def_test;

use super::{
    chain::{Chain, FilterHook},
    config::FilterConfig,
    matcher::{BaseMatch, InterfacePattern, Negated},
    rule::{Rule, Target, Verdict},
    table::FilterTable,
};
use crate::{
    buf::{PacketBuf, PacketOwner},
    device::{LinkKind, LinkSendSnapshot, LinkSnapshot, LoopbackDevice, NetDevice, RxPoll},
    ip::{IpAddress, Ipv4Address, Ipv4Cidr},
    ipv4,
    network::{interface_address::Ipv4AddrEntry, router::ROUTE_SCOPE_UNIVERSE},
    stack::router::Router,
};

struct OutputDevice {
    link: LoopbackDevice,
    sent: Arc<Mutex<Vec<PacketBuf>>>,
}
impl NetDevice for OutputDevice {
    fn name(&self) -> &str {
        self.link.name()
    }

    fn link_kind(&self) -> LinkKind {
        LinkKind::Ethernet
    }

    fn mtu(&self) -> usize {
        68
    }

    fn is_link_up(&self) -> bool {
        true
    }

    fn link_snapshot(&self, id: i32) -> LinkSnapshot {
        self.link.link_snapshot(id)
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
        _: IpAddress,
        packet: PacketBuf,
        _: MonotonicInstant,
    ) -> bool {
        self.sent.lock().push(packet);
        false
    }

    fn register_rx_waker(
        &self,
        w: &core::task::Waker,
        c: &mut PollContext<'_>,
    ) -> Result<(), PollRegisterError> {
        self.link.register_rx_waker(w, c)
    }
}
fn setup() -> (Router, Arc<Mutex<Vec<PacketBuf>>>) {
    let mut router = Router::new();
    let sent = Arc::new(Mutex::new(Vec::new()));
    let id = router
        .add_device(
            KBox::new(OutputDevice {
                link: LoopbackDevice::new(),
                sent: sent.clone(),
            })
            .unwrap(),
        )
        .unwrap();
    router
        .add_ipv4_addr(
            id,
            Ipv4AddrEntry {
                addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 1), 24).unwrap(),
                scope: ROUTE_SCOPE_UNIVERSE,
                broadcast: None,
                no_prefix_route: false,
                label: None,
            },
        )
        .unwrap();
    (router, sent)
}
fn config(policy: Verdict, rules: Vec<Rule>) -> FilterConfig {
    FilterConfig::new(
        FilterTable::new(vec![
            Chain::built_in(FilterHook::LocalIn, Verdict::Accept, vec![]),
            Chain::built_in(FilterHook::Forward, Verdict::Accept, vec![]),
            Chain::built_in(FilterHook::LocalOut, policy, rules),
        ])
        .unwrap(),
    )
}
fn packet(protocol: u8, dst: Ipv4Address) -> PacketBuf {
    PacketBuf::from_ip_packet_vec(
        0,
        ipv4::build_ipv4_packet(Ipv4Address::new(192, 0, 2, 1), dst, protocol, 64, &[0; 120])
            .unwrap(),
        PacketOwner::Ipv4Stack,
    )
}
#[def_test]
fn output_drops_all_local_protocols_before_device_send() {
    for protocol in [
        ipv4::PROTOCOL_UDP,
        ipv4::PROTOCOL_TCP,
        ipv4::PROTOCOL_ICMP,
        253,
    ] {
        let (mut router, sent) = setup();
        let filter = config(Verdict::Drop, vec![]);
        let bytes = packet(protocol, Ipv4Address::new(192, 0, 2, 2))
            .network_packet()
            .unwrap()
            .to_vec();
        match protocol {
            ipv4::PROTOCOL_ICMP => router.queue_control_ipv4_packet(bytes).unwrap(),
            ipv4::PROTOCOL_TCP => {
                use smoltcp::phy::{Device, TxToken};
                router
                    .stack_device()
                    .transmit(smoltcp::time::Instant::from_millis(0))
                    .unwrap()
                    .consume(bytes.len(), |data| data.copy_from_slice(&bytes));
            }
            _ => router.queue_ipv4_packet(bytes, 0).unwrap(),
        }
        assert_eq!(
            router
                .dispatch_output_budgeted(MonotonicInstant::ORIGIN, 1, &filter)
                .0,
            1
        );
        assert!(sent.lock().is_empty());
        filter.with_state(|s| {
            let c = s.collect_counters();
            assert_eq!(c[2].end.packets, 1);
            assert_eq!(c[2].end.bytes, 140);
            assert_eq!(c[0].end.packets, 0);
        });
    }
}
#[def_test]
fn output_matches_egress_and_counts_before_fragmentation() {
    let (mut router, sent) = setup();
    let rule = Rule::new(
        BaseMatch {
            input_interface: Some(Negated::new(
                InterfacePattern::exact("".to_string()).unwrap(),
                false,
            )),
            output_interface: Some(Negated::new(
                InterfacePattern::exact("lo".to_string()).unwrap(),
                false,
            )),
            ..Default::default()
        },
        vec![],
        Target::Verdict(Verdict::Accept),
    )
    .unwrap();
    let filter = config(Verdict::Drop, vec![rule]);
    router.dispatch_ipv4_output(
        packet(ipv4::PROTOCOL_TCP, Ipv4Address::new(192, 0, 2, 2)),
        MonotonicInstant::ORIGIN,
        &filter,
    );
    assert!(sent.lock().len() > 1);
    for p in sent.lock().iter() {
        let h = ipv4::Ipv4Header::parse_input(p.network_packet().unwrap()).unwrap();
        assert!(h.is_fragmented());
        assert!(h.total_len() <= 68);
    }
    filter.with_state(|s| {
        let c = s.collect_counters();
        assert_eq!(c[2].rules[0].packets, 1);
        assert_eq!(c[2].rules[0].bytes, 140);
    });
}
#[def_test]
fn broadcast_output_checks_each_selected_device() {
    let (mut router, sent) = setup();
    let mut link = LoopbackDevice::new();
    link.set_name("other".to_string());
    let other = router
        .add_device(
            KBox::new(OutputDevice {
                link,
                sent: sent.clone(),
            })
            .unwrap(),
        )
        .unwrap();
    let rule = Rule::new(
        BaseMatch {
            output_interface: Some(Negated::new(
                InterfacePattern::exact("lo".to_string()).unwrap(),
                false,
            )),
            ..Default::default()
        },
        vec![],
        Target::Verdict(Verdict::Accept),
    )
    .unwrap();
    let filter = config(Verdict::Drop, vec![rule]);
    router.dispatch_ipv4_output(
        packet(ipv4::PROTOCOL_UDP, Ipv4Address::BROADCAST),
        MonotonicInstant::ORIGIN,
        &filter,
    );
    let sent_count = sent.lock().len();
    assert!(sent_count > 1);
    filter.with_state(|s| {
        let counters = s.collect_counters();
        assert_eq!(counters[2].rules[0].packets, 1);
        assert_eq!(counters[2].end.packets, 1);
    });
    let mut bound = packet(ipv4::PROTOCOL_UDP, Ipv4Address::BROADCAST);
    bound.set_ifindex(other.ifindex());
    router.dispatch_ipv4_output(bound, MonotonicInstant::ORIGIN, &filter);
    assert_eq!(sent.lock().len(), sent_count);
    filter.with_state(|s| assert_eq!(s.collect_counters()[2].end.packets, 2));
}

#[def_test]
fn queued_output_counts_only_dequeued_datagrams() {
    let (mut router, sent) = setup();
    let filter = config(Verdict::Accept, vec![]);
    let make = || {
        packet(ipv4::PROTOCOL_TCP, Ipv4Address::new(192, 0, 2, 2))
            .network_packet()
            .unwrap()
            .to_vec()
    };
    while router.queue_ipv4_packet(make(), 0).is_ok() {}
    filter.with_state(|s| assert_eq!(s.collect_counters()[2].end.packets, 0));
    assert_eq!(
        router
            .dispatch_output_budgeted(MonotonicInstant::ORIGIN, 1, &filter)
            .0,
        1
    );
    let fragments = sent.lock().len();
    assert!(fragments > 1);
    filter.with_state(|s| assert_eq!(s.collect_counters()[2].end.packets, 1));
    assert_eq!(
        router
            .dispatch_output_budgeted(MonotonicInstant::ORIGIN, 0, &filter)
            .0,
        0
    );
    assert_eq!(sent.lock().len(), fragments);
    filter.with_state(|s| assert_eq!(s.collect_counters()[2].end.packets, 1));
}

#[def_test(serial)]
fn loopback_output_drop_prevents_receive_work() {
    let mut router = Router::new();
    let id = router
        .add_device(KBox::new(LoopbackDevice::new()).unwrap())
        .unwrap();
    router
        .add_ipv4_addr(
            id,
            Ipv4AddrEntry {
                addr: Ipv4Cidr::new(Ipv4Address::LOOPBACK, 8).unwrap(),
                scope: ROUTE_SCOPE_UNIVERSE,
                broadcast: None,
                no_prefix_route: false,
                label: None,
            },
        )
        .unwrap();
    let bytes = ipv4::build_ipv4_packet(
        Ipv4Address::LOOPBACK,
        Ipv4Address::LOOPBACK,
        ipv4::PROTOCOL_TCP,
        64,
        &[0; 20],
    )
    .unwrap();
    let filter = config(Verdict::Drop, vec![]);
    let queued = crate::device::net_rx::queued_len();
    router.dispatch_ipv4_output(
        PacketBuf::from_ip_packet_vec(0, bytes, PacketOwner::Ipv4Stack),
        MonotonicInstant::ORIGIN,
        &filter,
    );
    assert_eq!(crate::device::net_rx::queued_len(), queued);
    filter.with_state(|s| assert_eq!(s.collect_counters()[2].end.packets, 1));
}
