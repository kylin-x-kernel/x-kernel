// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec;

use unittest::def_test;

use super::*;
use crate::device::IPV4_MIN_MTU;

// Serial: reads the shared NetRx queue length around the send.
#[def_test(serial)]
fn loopback_rechecks_current_mtu_before_enqueue() {
    let mut device = LoopbackDevice::new();
    device.set_mtu(IPV4_MIN_MTU).unwrap();
    let packet =
        PacketBuf::from_ip_packet_vec(0, vec![0; IPV4_MIN_MTU + 1], PacketOwner::Ipv4Stack);
    let queued_before = net_rx::queued_len();

    assert!(!device.send_ip_packet(
        1,
        IpAddress::Ipv4(crate::ip::Ipv4Address::new(127, 0, 0, 1)),
        IpAddress::Ipv4(crate::ip::Ipv4Address::new(127, 0, 0, 1)),
        packet,
        MonotonicInstant::ORIGIN,
    ));
    assert_eq!(net_rx::queued_len(), queued_before);
    assert!(!device.has_rx_work());
}

#[def_test]
fn loopback_link_state_updates_are_idempotent() {
    let mut device = LoopbackDevice::new();
    assert_eq!(device.mtu(), LOOPBACK_MAX_MTU);

    device.set_link_up(false);
    device.set_link_up(false);
    let down = device.link_snapshot(1);
    assert_eq!(down.flags & LINK_FLAG_UP, 0);
    assert_eq!(down.operstate, IF_OPER_DOWN);

    device.set_link_up(true);
    device.set_link_up(true);
    let up = device.link_snapshot(1);
    assert_ne!(up.flags & LINK_FLAG_UP, 0);
    assert_eq!(up.operstate, IF_OPER_UNKNOWN);
}
