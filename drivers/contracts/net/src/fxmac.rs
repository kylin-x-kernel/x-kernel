// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Phytium FXMAC network driver adapter.
use alloc::{boxed::Box, collections::VecDeque, vec, vec::Vec};
use core::ptr::NonNull;

use driver_base::{Device, DeviceKind, DriverError, DriverResult};
pub use fxmac_rs::KernelFunc;
use fxmac_rs::{self, FXmac, FXmacGetMacAddress, FXmacLwipPortTx, FXmacRecvHandler, xmac_init};
use kspin::SpinNoIrq;
use log::*;

use crate::{MacAddress, NetBufHandle, NetDevice};

const QS: usize = 64;

/// FXMAC NIC driver instance.
///
/// The hardware device and the RX staging queue are each guarded by a
/// `SpinNoIrq` lock so the driver can be shared behind a `&self`
/// interface. The lock order is always RX queue first, then the device.
pub struct FXmacNic {
    inner: SpinNoIrq<usize>,
    hwaddr: [u8; 6],
    rx_buffer_queue: SpinNoIrq<VecDeque<NetBufHandle>>,
}

impl FXmacNic {
    /// Initialize the FXMAC driver instance.
    pub fn init(mapped_regs: usize) -> DriverResult<Self> {
        info!("FXmacNic init @ {mapped_regs:#x}");
        let rx_buffer_queue = VecDeque::with_capacity(QS);

        let mut hwaddr: [u8; 6] = [0; 6];
        FXmacGetMacAddress(&mut hwaddr, 0);
        info!("Got FXmac HW address: {hwaddr:x?}");

        let inner = xmac_init(&hwaddr);
        let dev = Self {
            inner: SpinNoIrq::new(inner as *mut FXmac as usize),
            hwaddr,
            rx_buffer_queue: SpinNoIrq::new(rx_buffer_queue),
        };
        Ok(dev)
    }

    fn with_inner<R>(&self, f: impl FnOnce(&mut FXmac) -> R) -> R {
        let ptr = *self.inner.lock() as *mut FXmac;
        // SAFETY: `xmac_init` returns a stable device instance that lives for
        // the lifetime of the driver. All mutable access is serialized by
        // `self.inner`, so we only materialize one `&mut FXmac` at a time.
        f(unsafe { &mut *ptr })
    }
}

impl Device for FXmacNic {
    fn name(&self) -> &str {
        "cdns,phytium-gem-1.0"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Net
    }
}

impl NetDevice for FXmacNic {
    fn mac(&self) -> MacAddress {
        MacAddress(self.hwaddr)
    }

    fn rx_queue_len(&self) -> usize {
        QS
    }

    fn tx_queue_len(&self) -> usize {
        QS
    }

    fn can_rx(&self) -> bool {
        !self.rx_buffer_queue.lock().is_empty()
    }

    fn can_tx(&self) -> bool {
        true
    }

    fn recycle_rx(&self, rx_buf: NetBufHandle) -> DriverResult {
        unsafe {
            drop(Box::from_raw(rx_buf.owner_ptr::<Vec<u8>>()));
        }
        Ok(())
    }

    fn recycle_tx(&self) -> DriverResult {
        // drop tx_buf
        Ok(())
    }

    fn recv(&self) -> DriverResult<NetBufHandle> {
        let mut rx_buffer_queue = self.rx_buffer_queue.lock();
        if !rx_buffer_queue.is_empty() {
            // RX buffer have received packets.
            Ok(rx_buffer_queue.pop_front().unwrap())
        } else {
            match self.with_inner(FXmacRecvHandler) {
                None => Err(DriverError::WouldBlock),
                Some(packets) => {
                    for payload in packets {
                        debug!("received payload length {}", payload.len());
                        let mut buf = Box::new(payload);
                        let buf_ptr = buf.as_mut_ptr();
                        let buf_len = buf.len();
                        let rx_buf = NetBufHandle::new(
                            NonNull::new(Box::into_raw(buf) as *mut u8).unwrap(),
                            NonNull::new(buf_ptr).unwrap(),
                            buf_len,
                        );

                        rx_buffer_queue.push_back(rx_buf);
                    }

                    Ok(rx_buffer_queue.pop_front().unwrap())
                }
            }
        }
    }

    fn send(&self, tx_buf: NetBufHandle) -> DriverResult {
        let tx_vec = vec![tx_buf.data().to_vec()];
        let ret = self.with_inner(|inner| FXmacLwipPortTx(inner, tx_vec));
        unsafe {
            drop(Box::from_raw(tx_buf.owner_ptr::<Vec<u8>>()));
        }
        if ret < 0 {
            Err(DriverError::WouldBlock)
        } else {
            Ok(())
        }
    }

    fn alloc_tx_buf(&self, size: usize) -> DriverResult<NetBufHandle> {
        let mut tx_buf = Box::new(alloc::vec![0; size]);
        let tx_buf_ptr = tx_buf.as_mut_ptr();

        Ok(NetBufHandle::new(
            NonNull::new(Box::into_raw(tx_buf) as *mut u8).unwrap(),
            NonNull::new(tx_buf_ptr).unwrap(),
            size,
        ))
    }
}

unittest::path_mod_test!(pub mod tests_fxmac, "../ut-tests/fxmac.rs");
