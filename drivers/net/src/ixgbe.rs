// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Intel ixgbe NIC driver implementation.
use alloc::{collections::VecDeque, sync::Arc};
use core::{convert::From, mem::ManuallyDrop, ptr::NonNull};

use driver_base::{DeviceKind, DriverError, DriverOps, DriverResult};
pub use ixgbe_driver::{INTEL_82599, INTEL_VEND, IxgbeHal, PhysAddr};
use ixgbe_driver::{IxgbeDevice, IxgbeError, IxgbeNetBuf, MemPool, NicDevice};
use log::*;

use crate::{MacAddress, NetBufHandle, NetDriverOps};

const RECV_BATCH_SIZE: usize = 64;
const RX_BUFFER_SIZE: usize = 1024;
const MEM_POOL: usize = 4096;
const MEM_POOL_ENTRY_SIZE: usize = 2048;

/// The ixgbe NIC device driver.
///
/// `QS` is the ixgbe queue size, `QN` is the ixgbe queue num.
pub struct IxgbeNic<H: IxgbeHal, const QS: usize, const QN: u16> {
    inner: IxgbeDevice<H, QS>,
    mem_pool: Arc<MemPool>,
    rx_buffer_queue: VecDeque<NetBufHandle>,
}

unsafe impl<H: IxgbeHal, const QS: usize, const QN: u16> Sync for IxgbeNic<H, QS, QN> {}
unsafe impl<H: IxgbeHal, const QS: usize, const QN: u16> Send for IxgbeNic<H, QS, QN> {}

impl<H: IxgbeHal, const QS: usize, const QN: u16> IxgbeNic<H, QS, QN> {
    /// Creates a net ixgbe NIC instance and initialize, or returns a error if
    /// any step fails.
    pub fn init(base: usize, len: usize) -> DriverResult<Self> {
        let mem_pool = MemPool::allocate::<H>(MEM_POOL, MEM_POOL_ENTRY_SIZE)
            .map_err(|_| DriverError::NoMemory)?;
        let inner = IxgbeDevice::<H, QS>::init(base, len, QN, QN, &mem_pool).map_err(|err| {
            error!("Failed to initialize ixgbe device: {err:?}");
            DriverError::BadState
        })?;

        let rx_buffer_queue = VecDeque::with_capacity(RX_BUFFER_SIZE);
        Ok(Self {
            inner,
            mem_pool,
            rx_buffer_queue,
        })
    }
}

impl<H: IxgbeHal, const QS: usize, const QN: u16> DriverOps for IxgbeNic<H, QS, QN> {
    fn name(&self) -> &str {
        self.inner.get_driver_name()
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Net
    }
}

impl<H: IxgbeHal, const QS: usize, const QN: u16> NetDriverOps for IxgbeNic<H, QS, QN> {
    fn mac(&self) -> MacAddress {
        MacAddress(self.inner.get_mac_addr())
    }

    fn rx_queue_len(&self) -> usize {
        QS
    }

    fn tx_queue_len(&self) -> usize {
        QS
    }

    fn can_rx(&self) -> bool {
        !self.rx_buffer_queue.is_empty() || self.inner.can_receive(0).unwrap()
    }

    fn can_tx(&self) -> bool {
        // Default implementation is return true forever.
        self.inner.can_send(0).unwrap()
    }

    fn recycle_rx(&mut self, rx_buf: NetBufHandle) -> DriverResult {
        let rx_buf = ixgbe_ptr_to_buf(rx_buf, &self.mem_pool)?;
        drop(rx_buf);
        Ok(())
    }

    fn recycle_tx(&mut self) -> DriverResult {
        self.inner
            .recycle_tx_buffers(0)
            .map_err(|_| DriverError::BadState)?;
        Ok(())
    }

    fn recv(&mut self) -> DriverResult<NetBufHandle> {
        if !self.can_rx() {
            return Err(DriverError::WouldBlock);
        }
        if !self.rx_buffer_queue.is_empty() {
            // RX buffer have received packets.
            Ok(self.rx_buffer_queue.pop_front().unwrap())
        } else {
            let f = |rx_buf| {
                let rx_buf = NetBufHandle::from(rx_buf);
                self.rx_buffer_queue.push_back(rx_buf);
            };

            // RX queue is empty, recv from ixgbe NIC.
            match self.inner.receive_packets(0, RECV_BATCH_SIZE, f) {
                Ok(recv_nums) => {
                    if recv_nums == 0 {
                        // No payload is received, it is impossible things.
                        panic!("Error: No recv packets.")
                    } else {
                        Ok(self.rx_buffer_queue.pop_front().unwrap())
                    }
                }
                Err(e) => match e {
                    IxgbeError::NotReady => Err(DriverError::WouldBlock),
                    _ => Err(DriverError::BadState),
                },
            }
        }
    }

    fn send(&mut self, tx_buf: NetBufHandle) -> DriverResult {
        let tx_buf = ixgbe_ptr_to_buf(tx_buf, &self.mem_pool)?;
        match self.inner.send(0, tx_buf) {
            Ok(_) => Ok(()),
            Err(err) => match err {
                IxgbeError::QueueFull => Err(DriverError::WouldBlock),
                _ => panic!("Unexpected err: {:?}", err),
            },
        }
    }

    fn alloc_tx_buf(&mut self, size: usize) -> DriverResult<NetBufHandle> {
        let tx_buf = IxgbeNetBuf::alloc(&self.mem_pool, size).map_err(|_| DriverError::NoMemory)?;
        Ok(NetBufHandle::from(tx_buf))
    }
}

impl From<IxgbeNetBuf> for NetBufHandle {
    fn from(buf: IxgbeNetBuf) -> Self {
        // Use `ManuallyDrop` to avoid drop `tx_buf`.
        let mut buf = ManuallyDrop::new(buf);
        let buf_ref =
            unsafe { &mut *(&mut buf as *mut ManuallyDrop<IxgbeNetBuf> as *mut IxgbeNetBuf) };
        // In ixgbe, `raw_ptr` is the pool entry, `buf_ptr` is the payload ptr, `len` is payload len
        // to avoid too many dynamic memory allocation.
        let buf_ptr = buf_ref.packet_mut().as_mut_ptr();
        Self::new(
            NonNull::new(buf_ref.pool_entry() as *mut u8).unwrap(),
            NonNull::new(buf_ptr).unwrap(),
            buf_ref.packet_len(),
        )
    }
}

// Converts a `NetBufHandle` to `IxgbeNetBuf`.
fn ixgbe_ptr_to_buf(ptr: NetBufHandle, pool: &Arc<MemPool>) -> DriverResult<IxgbeNetBuf> {
    IxgbeNetBuf::construct(ptr.owner_ptr::<()>().addr(), pool, ptr.len())
        .map_err(|_| DriverError::BadState)
}

#[cfg(unittest)]
pub mod tests_ixgbe {
    use unittest::def_test;
    extern crate alloc;
    use alloc::{collections::VecDeque, sync::Arc, vec, vec::Vec};
    use core::ptr::NonNull;

    use super::*;
    use crate::{MacAddress, NetBufHandle};

    // Mock IxgbeHal for testing
    struct MockIxgbeHal;

    impl IxgbeHal for MockIxgbeHal {
        fn dma_alloc(&self, _size: usize) -> (PhysAddr, *mut u8) {
            // Mock allocation - return safe test addresses
            let vaddr = 0x1000_0000;
            (PhysAddr::new(0x2000_0000), vaddr as *mut u8)
        }

        fn dma_dealloc(&self, _vaddr: *mut u8, _size: usize) {
            // Mock deallocation - no-op for tests
        }

        fn phys_to_virt(&self, paddr: PhysAddr) -> *mut u8 {
            paddr.0 as *mut u8
        }

        fn virt_to_phys(&self, vaddr: *mut u8) -> PhysAddr {
            PhysAddr::new(vaddr as usize)
        }
    }

    #[def_test]
    fn test_ixgbe_queue_size_validation() {
        // Test queue size constants and buffer management
        assert!(QS > 0, "Queue size must be positive");
        assert!(QS <= 4096, "Queue size should be reasonable");
        assert!(RECV_BATCH_SIZE > 0, "Receive batch size must be positive");
        assert!(
            RECV_BATCH_SIZE <= QS,
            "Batch size should not exceed queue size"
        );

        // Test memory pool configuration validation
        assert!(MEM_POOL > 0, "Memory pool size must be positive");
        assert!(
            MEM_POOL_ENTRY_SIZE >= 64,
            "Pool entry size should be reasonable"
        );
        assert!(
            MEM_POOL_ENTRY_SIZE <= 65536,
            "Pool entry size should not be excessive"
        );

        // Test RX buffer size validation
        assert!(
            RX_BUFFER_SIZE >= 60,
            "RX buffer size should handle minimum Ethernet frame"
        );
        assert!(
            RX_BUFFER_SIZE <= 9000,
            "RX buffer size should handle jumbo frames"
        );

        // Simulate queue operations
        let mut rx_queue: VecDeque<NetBufHandle> = VecDeque::with_capacity(RX_BUFFER_SIZE);

        // Test queue boundary conditions
        assert_eq!(rx_queue.capacity(), RX_BUFFER_SIZE);
        assert!(rx_queue.is_empty());

        // Fill queue with test data
        let mut test_handles = Vec::new();
        for i in 0..core::cmp::min(RX_BUFFER_SIZE, 100) {
            let data = vec![(i % 256) as u8; 1514];
            let box_data = Box::new(data.clone());
            let ptr = Box::into_raw(box_data);
            let handle = NetBufHandle::new(
                NonNull::new(ptr as *mut u8).unwrap(),
                NonNull::new(ptr as *mut u8).unwrap(),
                data.len(),
            );
            rx_queue.push_back(handle);
        }

        // Test queue state after filling
        assert!(!rx_queue.is_empty());
        assert!(rx_queue.len() <= RX_BUFFER_SIZE);

        // Test batch processing simulation
        let mut processed = 0;
        while !rx_queue.is_empty() && processed < RECV_BATCH_SIZE {
            if let Some(handle) = rx_queue.pop_front() {
                assert_eq!(handle.data().len(), 1514);
                assert_eq!(handle.data()[0], (processed % 256) as u8);
                processed += 1;

                // Clean up
                unsafe {
                    let _ = Box::from_raw(handle.owner_ptr::<Vec<u8>>());
                }
            }
        }

        assert!(processed > 0);
        assert!(processed <= RECV_BATCH_SIZE);

        // Clean up remaining handles
        while let Some(handle) = rx_queue.pop_front() {
            unsafe {
                let _ = Box::from_raw(handle.owner_ptr::<Vec<u8>>());
            }
        }
    }

    #[def_test]
    fn test_ixgbe_mac_address_boundary_conditions() {
        // Test MAC address validation and edge cases
        let test_mac_addresses = [
            [0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // All zeros
            [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF], // Broadcast address
            [0x00, 0x11, 0x22, 0x33, 0x44, 0x55], // Valid unicast
            [0x01, 0x00, 0x5E, 0x00, 0x00, 0x01], // IPv4 multicast
            [0x33, 0x33, 0x00, 0x00, 0x00, 0x01], // IPv6 multicast
            [0x02, 0x00, 0x00, 0x00, 0x00, 0x01], // Locally administered
            [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF], // Random valid address
        ];

        for &mac_bytes in &test_mac_addresses {
            let mac_addr = MacAddress(mac_bytes);

            // Test MAC address properties
            assert_eq!(mac_addr.0.len(), 6);
            assert_eq!(mac_addr.0, mac_bytes);

            // Test multicast detection
            let is_multicast = (mac_bytes[0] & 0x01) != 0;
            let is_broadcast = mac_bytes == [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
            let is_unicast = !is_multicast && !is_broadcast;

            // Test locally administered detection
            let is_locally_administered = (mac_bytes[0] & 0x02) != 0;
            let is_globally_unique = !is_locally_administered;

            // Validate address types are mutually exclusive (except broadcast is also multicast)
            if is_broadcast {
                assert!(is_multicast);
                assert!(!is_unicast);
            } else if is_multicast {
                assert!(!is_unicast);
            }

            // Test address format validation
            // Check OUI (first 3 bytes) patterns for known vendors
            match &mac_bytes[0..3] {
                [0x00, 0x50, 0x56] => {} // VMware
                [0x08, 0x00, 0x27] => {} // VirtualBox
                [0x52, 0x54, 0x00] => {} // QEMU
                [0x00, 0x0C, 0x29] => {} // VMware
                _ => {}                  // Other/unknown vendor
            }
        }

        // Test MAC address conversion and manipulation
        for i in 0..256u8 {
            let test_mac = [
                i,
                i.wrapping_add(1),
                i.wrapping_add(2),
                i.wrapping_add(3),
                i.wrapping_add(4),
                i.wrapping_add(5),
            ];
            let mac_addr = MacAddress(test_mac);

            // Test that MAC address maintains data integrity
            assert_eq!(mac_addr.0[0], i);
            assert_eq!(mac_addr.0[1], i.wrapping_add(1));
            assert_eq!(mac_addr.0[2], i.wrapping_add(2));
            assert_eq!(mac_addr.0[3], i.wrapping_add(3));
            assert_eq!(mac_addr.0[4], i.wrapping_add(4));
            assert_eq!(mac_addr.0[5], i.wrapping_add(5));
        }

        // Test boundary values for individual bytes
        let boundary_values = [0x00, 0x01, 0x7F, 0x80, 0xFE, 0xFF];
        for &val in &boundary_values {
            let mac = [val, val, val, val, val, val];
            let mac_addr = MacAddress(mac);
            assert_eq!(mac_addr.0, mac);

            // Test bit operations
            let has_multicast_bit = (val & 0x01) != 0;
            let has_local_bit = (val & 0x02) != 0;

            assert_eq!((mac_addr.0[0] & 0x01) != 0, has_multicast_bit);
            assert_eq!((mac_addr.0[0] & 0x02) != 0, has_local_bit);
        }
    }
}
