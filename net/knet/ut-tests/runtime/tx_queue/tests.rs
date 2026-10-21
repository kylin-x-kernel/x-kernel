// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{vec, vec::Vec};

use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::buf::PacketOwner;

#[def_test]
fn data_tx_queue_preserves_rejected_packets_and_enforces_capacity() {
    let packet_fn =
        |marker| PacketBuf::from_ip_packet_vec(1, vec![marker], PacketOwner::Ipv4Stack);
    let mut queue = DataTxQueue::with_capacity(2);
    assert!(queue.try_push(packet_fn(0)).is_ok());
    assert!(!queue.can_push(usize::MAX));

    let first = packet_fn(1);
    let second = packet_fn(2);
    let rejected = queue
        .try_extend(vec![first.clone(), second.clone()].into_iter())
        .err()
        .unwrap()
        .collect::<Vec<_>>();
    assert_eq!(queue.available(), 1);
    assert_eq!(rejected.len(), 2);
    assert!(rejected[0].shares_storage_with(&first));
    assert!(rejected[1].shares_storage_with(&second));

    let queued = queue.pop_front().unwrap();
    assert_eq!(queued.network_packet(), Some(&[0][..]));
    assert!(queue.try_extend(rejected.into_iter()).is_ok());
    assert!(queue.is_full());
    let extra = packet_fn(3);
    let rejected = queue.try_push(extra.clone()).err().unwrap();
    assert!(rejected.shares_storage_with(&extra));
    assert_eq!(queue.available(), 0);
    assert!(queue.try_extend(core::iter::empty()).is_ok());
    assert!(queue.pop_front().unwrap().shares_storage_with(&first));
    assert!(queue.pop_front().unwrap().shares_storage_with(&second));
    assert!(queue.is_empty());
}
