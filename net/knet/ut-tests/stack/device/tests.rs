// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use smoltcp::phy::{Device, RxToken as _, TxToken as _};
use unittest::def_test;

use super::*;

#[def_test]
fn tx_backpressure_preserves_rx_until_a_response_slot_is_available() {
    let mut rx = VecDeque::from([PacketBuf::from_ip_packet_vec(
        1,
        vec![0x45, 1],
        PacketOwner::DeviceRx,
    )]);
    let mut tx = DataTxQueue::with_capacity(1);
    tx.try_push(PacketBuf::from_ip_packet_vec(
        0,
        vec![0],
        PacketOwner::Ipv4Stack,
    ))
    .unwrap();
    let now = Instant::from_millis(0);
    {
        let mut device = StackDevice::new(&mut rx, &mut tx, 1500);
        assert!(device.receive(now).is_none());
        assert!(device.transmit(now).is_none());
    }
    assert_eq!(rx.len(), 1);
    tx.pop_front().unwrap();
    {
        let mut device = StackDevice::new(&mut rx, &mut tx, 1500);
        let (received, response) = device.receive(now).unwrap();
        received.consume(|bytes| assert_eq!(bytes, &[0x45, 1]));
        response.consume(2, |bytes| bytes.copy_from_slice(&[0x45, 2]));
    }
    assert!(rx.is_empty());
    assert_eq!(
        tx.pop_front().unwrap().network_packet(),
        Some(&[0x45, 2][..])
    );
}

#[def_test]
fn dropping_a_tx_token_does_not_consume_queue_capacity() {
    let mut rx = VecDeque::new();
    let mut tx = DataTxQueue::with_capacity(1);
    {
        let mut device = StackDevice::new(&mut rx, &mut tx, 9000);
        let caps = device.capabilities();
        assert_eq!(caps.medium, Medium::Ip);
        assert_eq!(caps.max_transmission_unit, 9000);
        assert_eq!(caps.max_burst_size, Some(SOCKET_BUFFER_SIZE));
        assert!(device.transmit(Instant::from_millis(0)).is_some());
    }
    assert_eq!(tx.available(), 1);
}
