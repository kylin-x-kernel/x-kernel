// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

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

#[cfg(unittest)]
pub mod tests_fxmac {
    use unittest::def_test;
    extern crate alloc;
    use alloc::{boxed::Box, collections::VecDeque, vec, vec::Vec};
    use core::ptr::NonNull;

    use super::*;
    use crate::{MacAddress, NetBufHandle};

    // Mock FXmac structure for testing
    struct MockFXmac;

    #[def_test]
    fn test_fxmac_queue_management() {
        // Test RX buffer queue operations with boundary conditions
        let mut rx_queue: VecDeque<NetBufHandle> = VecDeque::with_capacity(QS);

        // Test queue capacity and boundary operations
        assert_eq!(rx_queue.capacity(), QS);
        assert!(rx_queue.is_empty());

        // Fill queue to capacity
        let test_data_sets = (0..QS)
            .map(|i| {
                let mut data = vec![0u8; 1514]; // Standard Ethernet frame size
                data.fill(i as u8);
                data
            })
            .collect::<Vec<_>>();

        let handles: Vec<NetBufHandle> = test_data_sets
            .iter()
            .map(|data| {
                let box_data = Box::new(data.clone());
                let ptr = Box::into_raw(box_data);
                NetBufHandle::new(
                    NonNull::new(ptr as *mut u8).unwrap(),
                    NonNull::new(ptr as *mut u8).unwrap(),
                    data.len(),
                )
            })
            .collect();

        // Add all handles to queue
        for handle in handles {
            rx_queue.push_back(handle);
        }

        assert_eq!(rx_queue.len(), QS);
        assert!(!rx_queue.is_empty());

        // Test queue operations at boundaries
        // Remove half the packets
        let mut removed_handles = Vec::new();
        for _ in 0..QS / 2 {
            if let Some(handle) = rx_queue.pop_front() {
                // Verify data integrity
                assert_eq!(handle.data().len(), 1514);
                removed_handles.push(handle);
            }
        }

        assert_eq!(rx_queue.len(), QS - QS / 2);

        // Test edge case: try to add more packets when queue has space
        for i in 0..5 {
            let data = vec![(200 + i) as u8; 60]; // Minimum Ethernet frame
            let box_data = Box::new(data.clone());
            let ptr = Box::into_raw(box_data);
            let handle = NetBufHandle::new(
                NonNull::new(ptr as *mut u8).unwrap(),
                NonNull::new(ptr as *mut u8).unwrap(),
                data.len(),
            );
            rx_queue.push_back(handle);
        }

        // Verify queue behavior under stress
        assert!(rx_queue.len() <= QS);

        // Clean up remaining handles
        while let Some(handle) = rx_queue.pop_front() {
            unsafe {
                let _ = Box::from_raw(handle.owner_ptr::<Vec<u8>>());
            }
        }

        for handle in removed_handles {
            unsafe {
                let _ = Box::from_raw(handle.owner_ptr::<Vec<u8>>());
            }
        }
    }

    #[def_test]
    fn test_fxmac_network_frame_validation() {
        // Test network frame size validation and edge cases
        let frame_sizes = [
            60,    // Minimum Ethernet frame (without CRC)
            64,    // Minimum with CRC
            1514,  // Standard maximum
            1518,  // Maximum with CRC
            9000,  // Jumbo frame
            65535, // Maximum possible
        ];

        for frame_size in frame_sizes {
            // Create test frame with specific size
            let mut frame_data = vec![0u8; frame_size];

            // Fill with Ethernet-like pattern
            if frame_data.len() >= 14 {
                // Destination MAC (6 bytes)
                frame_data[0..6].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
                // Source MAC (6 bytes)
                frame_data[6..12].copy_from_slice(&[0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
                // EtherType (2 bytes) - IPv4
                frame_data[12..14].copy_from_slice(&[0x08, 0x00]);
            }

            // Fill payload with test pattern
            for (i, byte) in frame_data.iter_mut().enumerate().skip(14) {
                *byte = (i % 256) as u8;
            }

            // Create NetBufHandle for frame
            let box_data = Box::new(frame_data.clone());
            let ptr = Box::into_raw(box_data);
            let handle = NetBufHandle::new(
                NonNull::new(ptr as *mut u8).unwrap(),
                NonNull::new(ptr as *mut u8).unwrap(),
                frame_data.len(),
            );

            // Validate frame properties
            assert_eq!(handle.len(), frame_size);
            assert_eq!(handle.data().len(), frame_size);
            assert!(!handle.is_empty() || frame_size == 0);

            // Test frame content validation
            let data = handle.data();
            if data.len() >= 14 {
                // Check destination MAC
                assert_eq!(&data[0..6], &[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
                // Check source MAC
                assert_eq!(&data[6..12], &[0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
                // Check EtherType
                assert_eq!(&data[12..14], &[0x08, 0x00]);

                // Verify payload pattern
                for (i, &byte) in data.iter().enumerate().skip(14) {
                    assert_eq!(byte, (i % 256) as u8);
                }
            }

            // Test MAC address extraction simulation
            if data.len() >= 12 {
                let src_mac: [u8; 6] = [data[6], data[7], data[8], data[9], data[10], data[11]];
                let mac_addr = MacAddress(src_mac);
                assert_eq!(mac_addr.0, [0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
            }

            // Clean up
            unsafe {
                let _ = Box::from_raw(handle.owner_ptr::<Vec<u8>>());
            }
        }

        // Test invalid frame sizes (too small)
        let invalid_sizes = [0, 1, 30, 59]; // Below minimum Ethernet frame size
        for invalid_size in invalid_sizes {
            let frame_data = vec![0u8; invalid_size];
            let box_data = Box::new(frame_data.clone());
            let ptr = Box::into_raw(box_data);
            let handle = NetBufHandle::new(
                NonNull::new(ptr as *mut u8).unwrap(),
                NonNull::new(ptr as *mut u8).unwrap(),
                invalid_size,
            );

            // These should still create valid handles, but represent invalid Ethernet frames
            assert_eq!(handle.len(), invalid_size);
            assert_eq!(handle.is_empty(), invalid_size == 0);

            // Clean up
            unsafe {
                let _ = Box::from_raw(handle.owner_ptr::<Vec<u8>>());
            }
        }
    }
}
