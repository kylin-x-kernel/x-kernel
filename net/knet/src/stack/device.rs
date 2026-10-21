// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Queue-only smoltcp adapter; it has no route or physical-device access.

use alloc::{collections::VecDeque, vec};

use smoltcp::{
    phy::{DeviceCapabilities, Medium},
    time::Instant,
};

use crate::{
    buf::{PacketBuf, PacketOwner},
    consts::SOCKET_BUFFER_SIZE,
    runtime::tx_queue::DataTxQueue,
};

/// Borrows queue ownership for one synchronous smoltcp operation.
///
/// The caller serializes both queues for the adapter's entire lifetime. A TX
/// token retains the exclusive queue borrow through its capacity check and
/// enqueue, so another producer cannot consume the selected slot.
pub(crate) struct StackDevice<'a> {
    rx: &'a mut VecDeque<PacketBuf>,
    tx: &'a mut DataTxQueue,
    mtu: usize,
}

impl<'a> StackDevice<'a> {
    pub(super) fn new(
        rx: &'a mut VecDeque<PacketBuf>,
        tx: &'a mut DataTxQueue,
        mtu: usize,
    ) -> Self {
        Self { rx, tx, mtu }
    }
}

pub(crate) struct TxToken<'a>(&'a mut DataTxQueue);

impl smoltcp::phy::TxToken for TxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut ip_packet = vec![0u8; len];
        let result = f(&mut ip_packet);
        assert!(
            self.0
                .try_push(PacketBuf::from_ip_packet_vec(
                    0,
                    ip_packet,
                    PacketOwner::Ipv4Stack,
                ))
                .is_ok(),
            "smoltcp consumed a TX token without queue capacity"
        );
        result
    }
}

pub(crate) struct RxToken(PacketBuf);

impl smoltcp::phy::RxToken for RxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(self
            .0
            .network_packet()
            .expect("StackDevice RX queue only contains IP packets"))
    }
}

impl smoltcp::phy::Device for StackDevice<'_> {
    type RxToken<'a>
        = RxToken
    where
        Self: 'a;
    type TxToken<'a>
        = TxToken<'a>
    where
        Self: 'a;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if self.tx.is_full() {
            return None;
        }
        let packet = self.rx.pop_front()?;
        Some((RxToken(packet), TxToken(self.tx)))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        if self.tx.is_full() {
            None
        } else {
            Some(TxToken(self.tx))
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = self.mtu;
        caps.max_burst_size = Some(SOCKET_BUFFER_SIZE);
        caps
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/stack/device/tests.rs");
