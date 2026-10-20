// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Intel ixgbe NIC driver implementation.
use alloc::{collections::VecDeque, string::String, sync::Arc};
use core::{convert::From, mem::ManuallyDrop, ptr::NonNull};

use driver_base::{Device, DeviceKind, DriverError, DriverResult};
pub use ixgbe_driver::{INTEL_82599, INTEL_VEND, IxgbeHal, PhysAddr};
use ixgbe_driver::{IxgbeDevice, IxgbeError, IxgbeNetBuf, MemPool, NicDevice};
use kspin::SpinNoIrq;
use log::*;

use crate::{MacAddress, NetBufHandle, NetDevice};

const RECV_BATCH_SIZE: usize = 64;
const RX_BUFFER_SIZE: usize = 1024;
const MEM_POOL: usize = 4096;
const MEM_POOL_ENTRY_SIZE: usize = 2048;

/// The ixgbe NIC device driver.
///
/// `QS` is the ixgbe queue size, `QN` is the ixgbe queue num.
///
/// The hardware device and the RX staging queue are each guarded by a
/// `SpinNoIrq` lock so the driver can be shared behind a `&self`
/// interface. The lock order is always RX queue first, then the device.
pub struct IxgbeNic<H: IxgbeHal, const QS: usize, const QN: u16> {
    inner: SpinNoIrq<IxgbeDevice<H, QS>>,
    mem_pool: Arc<MemPool>,
    rx_buffer_queue: SpinNoIrq<VecDeque<NetBufHandle>>,
    name: String,
}

// SAFETY: the hardware device and RX staging queue are each protected by
// `SpinNoIrq`, and the driver maintains a consistent lock order, so shared
// references remain synchronized across threads.
unsafe impl<H: IxgbeHal, const QS: usize, const QN: u16> Sync for IxgbeNic<H, QS, QN> {}
// SAFETY: ownership may move across threads because all mutable interaction
// with device state and staged buffers remains serialized by the same
// `SpinNoIrq` locks.
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

        let name = String::from(inner.get_driver_name());
        let rx_buffer_queue = VecDeque::with_capacity(RX_BUFFER_SIZE);
        Ok(Self {
            inner: SpinNoIrq::new(inner),
            mem_pool,
            rx_buffer_queue: SpinNoIrq::new(rx_buffer_queue),
            name,
        })
    }
}

impl<H: IxgbeHal, const QS: usize, const QN: u16> Device for IxgbeNic<H, QS, QN> {
    fn name(&self) -> &str {
        &self.name
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Net
    }
}

impl<H: IxgbeHal, const QS: usize, const QN: u16> NetDevice for IxgbeNic<H, QS, QN> {
    fn mac(&self) -> MacAddress {
        MacAddress(self.inner.lock().get_mac_addr())
    }

    fn rx_queue_len(&self) -> usize {
        QS
    }

    fn tx_queue_len(&self) -> usize {
        QS
    }

    fn can_rx(&self) -> bool {
        !self.rx_buffer_queue.lock().is_empty() || self.inner.lock().can_receive(0).unwrap()
    }

    fn can_tx(&self) -> bool {
        // Default implementation is return true forever.
        self.inner.lock().can_send(0).unwrap()
    }

    fn recycle_rx(&self, rx_buf: NetBufHandle) -> DriverResult {
        let rx_buf = ixgbe_ptr_to_buf(rx_buf, &self.mem_pool)?;
        drop(rx_buf);
        Ok(())
    }

    fn recycle_tx(&self) -> DriverResult {
        self.inner
            .lock()
            .recycle_tx_buffers(0)
            .map_err(|_| DriverError::BadState)?;
        Ok(())
    }

    fn recv(&self) -> DriverResult<NetBufHandle> {
        // Hold the RX staging queue lock for the whole check-then-receive
        // sequence. Probing readiness outside the lock would race with a
        // concurrent `recv()` draining the queue, leaving this call to observe
        // an empty queue after `receive_packets` reports zero packets.
        let mut rx_buffer_queue = self.rx_buffer_queue.lock();
        if let Some(rx_buf) = rx_buffer_queue.pop_front() {
            // RX buffer have received packets.
            return Ok(rx_buf);
        }

        let f = |rx_buf| {
            let rx_buf = NetBufHandle::from(rx_buf);
            rx_buffer_queue.push_back(rx_buf);
        };

        // RX queue is empty, recv from ixgbe NIC.
        match self.inner.lock().receive_packets(0, RECV_BATCH_SIZE, f) {
            Ok(_) => rx_buffer_queue.pop_front().ok_or(DriverError::WouldBlock),
            Err(e) => match e {
                IxgbeError::NotReady => Err(DriverError::WouldBlock),
                _ => Err(DriverError::BadState),
            },
        }
    }

    fn send(&self, tx_buf: NetBufHandle) -> DriverResult {
        let tx_buf = ixgbe_ptr_to_buf(tx_buf, &self.mem_pool)?;
        match self.inner.lock().send(0, tx_buf) {
            Ok(_) => Ok(()),
            Err(err) => match err {
                IxgbeError::QueueFull => Err(DriverError::WouldBlock),
                _ => panic!("Unexpected err: {:?}", err),
            },
        }
    }

    fn alloc_tx_buf(&self, size: usize) -> DriverResult<NetBufHandle> {
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

unittest::path_mod_test!(pub mod tests_ixgbe, "../ut-tests/ixgbe.rs");
