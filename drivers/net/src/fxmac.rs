//! Phytium FXMAC network driver adapter.
use alloc::{boxed::Box, collections::VecDeque, vec, vec::Vec};
use core::ptr::NonNull;

use driver_base::{DeviceKind, DriverError, DriverOps, DriverResult};
pub use fxmac_rs::KernelFunc;
use fxmac_rs::{self, FXmac, FXmacGetMacAddress, FXmacLwipPortTx, FXmacRecvHandler, xmac_init};
use log::*;

use crate::{MacAddress, NetBufHandle, NetDriverOps};

const QS: usize = 64;

/// FXMAC NIC driver instance.
pub struct FXmacNic {
    inner: &'static mut FXmac,
    hwaddr: [u8; 6],
    rx_buffer_queue: VecDeque<NetBufHandle>,
}

unsafe impl Sync for FXmacNic {}
unsafe impl Send for FXmacNic {}

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
            inner,
            hwaddr,
            rx_buffer_queue,
        };
        Ok(dev)
    }
}

impl DriverOps for FXmacNic {
    fn name(&self) -> &str {
        "cdns,phytium-gem-1.0"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Net
    }
}

impl NetDriverOps for FXmacNic {
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
        !self.rx_buffer_queue.is_empty()
    }

    fn can_tx(&self) -> bool {
        true
    }

    fn recycle_rx(&mut self, rx_buf: NetBufHandle) -> DriverResult {
        unsafe {
            drop(Box::from_raw(rx_buf.owner_ptr::<Vec<u8>>()));
        }
        Ok(())
    }

    fn recycle_tx(&mut self) -> DriverResult {
        // drop tx_buf
        Ok(())
    }

    fn recv(&mut self) -> DriverResult<NetBufHandle> {
        if !self.rx_buffer_queue.is_empty() {
            // RX buffer have received packets.
            Ok(self.rx_buffer_queue.pop_front().unwrap())
        } else {
            match FXmacRecvHandler(self.inner) {
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

                        self.rx_buffer_queue.push_back(rx_buf);
                    }

                    Ok(self.rx_buffer_queue.pop_front().unwrap())
                }
            }
        }
    }

    fn send(&mut self, tx_buf: NetBufHandle) -> DriverResult {
        let tx_vec = vec![tx_buf.data().to_vec()];
        let ret = FXmacLwipPortTx(self.inner, tx_vec);
        unsafe {
            drop(Box::from_raw(tx_buf.owner_ptr::<Vec<u8>>()));
        }
        if ret < 0 {
            Err(DriverError::WouldBlock)
        } else {
            Ok(())
        }
    }

    fn alloc_tx_buf(&mut self, size: usize) -> DriverResult<NetBufHandle> {
        let mut tx_buf = Box::new(alloc::vec![0; size]);
        let tx_buf_ptr = tx_buf.as_mut_ptr();

        Ok(NetBufHandle::new(
            NonNull::new(Box::into_raw(tx_buf) as *mut u8).unwrap(),
            NonNull::new(tx_buf_ptr).unwrap(),
            size,
        ))
    }
}
