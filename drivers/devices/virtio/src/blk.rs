// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO block driver adapter.
use alloc::{string::String, sync::Arc};
use core::sync::atomic::{AtomicU32, Ordering};

use block::{
    BlockDeviceOperations,
    completion::{BlockSignals, PrepareBlockWait},
};
use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use kspin::SpinNoIrq;
use virtio_drivers::{
    Hal,
    device::blk::{SECTOR_SIZE, VirtIOBlk as InnerDev},
    transport::Transport,
};

use crate::as_driver_error;

mod requests;
use requests::BlockRequests;
#[cfg(unittest)]
pub use requests::requests_unittest::{
    Hardware as BlockTestHardware, QueueTransport as BlockTestTransport,
    TrackedHal as BlockTestHal, block_test_disk,
};

struct BlockState<H: Hal, T: Transport> {
    device: Option<InnerDev<H, T>>,
    requests: Option<BlockRequests>,
    is_accepting: bool,
    active_calls: usize,
    drain_notification: Option<Arc<dyn BlockSignals>>,
}

// Covers wait preparation as well as pending/submitted requests. The borrowed
// device remains alive until the call retires, even if public lookup is removed.
struct BlockCall<'a, H: Hal, T: Transport>(&'a VirtIoBlkDev<H, T>);

impl<H: Hal, T: Transport> Drop for BlockCall<'_, H, T> {
    fn drop(&mut self) {
        let notification = {
            let mut state = self.0.state.lock();
            assert!(state.active_calls > 0);
            state.active_calls -= 1;
            if state.active_calls == 0 && !state.is_accepting {
                state.drain_notification.take()
            } else {
                None
            }
        };
        if let Some(notification) = notification {
            notification.notify_completion();
        }
    }
}

/// Number of minor bits reserved for partitions of one virtio disk.
pub const PART_BITS: u32 = 4;
/// Block major used by the current X-Kernel virtio disk namespace.
pub const VIRTIO_BLK_MAJOR: u32 = 254;

/// Global virtio-blk index allocator.
///
/// Assigned in discovery order by [`VirtIoBlkDev::try_new`] and used to derive
/// the Linux-style `vdX` device name. `Relaxed` ordering suffices because each
/// device only needs a unique index, not a globally synchronized counter value.
static VD_INDEX: AtomicU32 = AtomicU32::new(0);

/// Converts a zero-based index into a base26 suffix for Linux-style
/// virtio-blk device names (0 -> "a", 25 -> "z", 26 -> "aa").
/// The caller will prepend the "vd" prefix separately.
fn vd_name(index: u32) -> String {
    let mut suffix = String::new();
    let mut n = index;
    loop {
        let digit = (n % 26) as u8;
        suffix.push((b'a' + digit) as char);
        n /= 26;
        if n == 0 {
            break;
        }
        n -= 1;
    }
    suffix.chars().rev().collect()
}

/// The VirtIO block device driver.
///
/// Wraps `VirtIOBlk` from `virtio-drivers` and implements the
/// [`BlockDeviceOperations`] trait, providing sector-level read/write access to a
/// virtual block device.
///
/// # Type Parameters
///
/// - `H` - VirtIO HAL implementation for DMA allocation.
/// - `T` - Transport layer (MMIO or PCI).
///
/// # Example
///
/// ```ignore
/// let (kind, transport) = virtio::probe_pci_device::<HalImpl, _>(...).unwrap();
/// let mut blk = VirtIoBlkDev::<HalImpl, _>::try_new(transport)?;
/// let mut buf = [0u8; 512];
/// blk.read_block(0, &mut buf)?;
/// ```
pub struct VirtIoBlkDev<H: Hal, T: Transport> {
    state: SpinNoIrq<BlockState<H, T>>,
    prepare_wait: Option<PrepareBlockWait>,
    num_blocks: u64,
    is_read_only: bool,
    name: String,
    index: u32,
}

// SAFETY: VirtIoBlkDev serializes all access to the inner VirtIOBlk through
// its own `SpinNoIrq` lock. The inner VirtIOBlk is not auto Send due to
// PhantomData, but it is safe to transfer across threads behind that lock.
// It also protects the intrusive request links, token pointers and request
// state in IRQ mode. Each pointer indexes a call-owned pinned node; terminal
// publication removes all external node references before that call returns.
// Legacy polling still masks interrupts to avoid shared-level IRQ livelock.
// IRQ mode holds this lock only for submission/ack/reclamation, never waiting.
unsafe impl<H: Hal, T: Transport> Send for VirtIoBlkDev<H, T> {}
// SAFETY: shared access to the device is serialized by the IRQ-safe lock
// described above, so immutable references may be shared across threads safely.
unsafe impl<H: Hal, T: Transport> Sync for VirtIoBlkDev<H, T> {}

impl<H: Hal, T: Transport> VirtIoBlkDev<H, T> {
    /// Creates a new driver instance and initializes the device, or returns
    /// an error if any step fails.
    ///
    /// # Errors
    ///
    /// Returns `DriverError` if the device fails to initialize (e.g. feature
    /// negotiation failure, queue allocation failure, DMA error).
    pub fn try_new(transport: T) -> DriverResult<Self> {
        let device = Self::init_device(transport)?;
        Ok(Self::from_device(device, None))
    }

    /// Prepares interrupt-driven request handling without publishing the disk.
    ///
    /// Call in task context. The host must install its shared IRQ handler and
    /// completion executor, then enable interrupts before allowing I/O. Setup
    /// failure must not fall back to the polling constructor. The host retains
    /// this device until all admitted calls and completion callbacks retire.
    ///
    /// # Errors
    /// Returns initialization errors from the transport, or Unsupported when
    /// built with unwinding: submitted stack nodes cannot unwind through DMA.
    pub fn try_new_irq(transport: T, prepare_wait: PrepareBlockWait) -> DriverResult<Self> {
        if cfg!(panic = "unwind") {
            return Err(DriverError::Unsupported);
        }
        let mut device = Self::init_device(transport)?;
        device.disable_interrupts();
        Ok(Self::from_device(device, Some(prepare_wait)))
    }

    fn from_device(device: InnerDev<H, T>, prepare_wait: Option<PrepareBlockWait>) -> Self {
        let num_blocks = device.capacity();
        let is_read_only = device.readonly();
        let index = VD_INDEX.fetch_add(1, Ordering::Relaxed);
        let requests = prepare_wait.map(|_| BlockRequests::new(device.virt_queue_size()));
        Self {
            state: SpinNoIrq::new(BlockState {
                device: Some(device),
                requests,
                is_accepting: true,
                active_calls: 0,
                drain_notification: None,
            }),
            prepare_wait,
            num_blocks,
            is_read_only,
            name: alloc::format!("vd{}", vd_name(index)),
            index,
        }
    }

    /// Enables queue interrupts after the host installed IRQ/completion handling.
    ///
    /// # Panics
    /// Panics if closing has begun or the transport has been destroyed.
    pub fn enable_interrupts(&self) {
        let mut state = self.state.lock();
        assert!(state.is_accepting);
        state
            .device
            .as_mut()
            .expect("live block transport")
            .enable_interrupts();
    }

    /// Suppresses queue interrupts after admitted I/O has drained during close.
    /// This does not unregister the host IRQ action or stop an in-flight callback.
    pub fn disable_interrupts(&self) {
        if let Some(device) = self.state.lock().device.as_mut() {
            device.disable_interrupts();
            device.ack_interrupt();
        }
    }

    /// Rejects new I/O and signals when every previously admitted call retires.
    ///
    /// The host serializes close, supplies a fresh closing-task notification,
    /// then withdraws disk lookup and waits without device/registry locks. Keep
    /// IRQ and completion processing active until the drain signal is observed.
    /// Live is accepting with a transport; Closing retains a transport without
    /// accepting; Closed has neither. This method does not destroy the transport.
    ///
    /// # Panics
    /// Panics if another close is already draining this device.
    pub fn begin_close(&self, notification: Arc<dyn BlockSignals>) {
        let is_drained = {
            let mut state = self.state.lock();
            assert!(
                state.is_accepting || state.device.is_none(),
                "concurrent block close"
            );
            state.is_accepting = false;
            if state.active_calls == 0 {
                true
            } else {
                state.drain_notification = Some(notification.clone());
                false
            }
        };
        if is_drained {
            notification.notify_completion();
        }
    }

    /// Destroys a drained transport in task context, leaving retained disks inert.
    ///
    /// The host must first suppress device interrupts, release/synchronize its
    /// native IRQ action and stop the device reclaimer. Never call in a callback.
    /// Repeated calls after close are harmless. Cached geometry remains available.
    ///
    /// # Panics
    /// Panics if admission is still open, calls remain active, or request
    /// reclamation has not finished.
    pub fn finish_close(&self) {
        let device = {
            let mut state = self.state.lock();
            assert!(!state.is_accepting && state.active_calls == 0);
            if let Some(requests) = &state.requests {
                assert!(requests.is_drained());
            }
            state.device.take()
        };
        // Transport reset and coherent queue deallocation require task context.
        drop(device);
    }

    fn admit_call(&self) -> DriverResult<BlockCall<'_, H, T>> {
        let mut state = self.state.lock();
        if !state.is_accepting {
            return Err(DriverError::Io);
        }
        state.active_calls = state
            .active_calls
            .checked_add(1)
            .expect("block call count overflow");
        Ok(BlockCall(self))
    }

    /// Returns the discovery-order disk index retained for driver cleanup and
    /// `gendisk` minor allocation.
    pub const fn index(&self) -> u32 {
        self.index
    }

    fn init_device(transport: T) -> DriverResult<InnerDev<H, T>> {
        InnerDev::new(transport).map_err(as_driver_error)
    }
}

impl<H: Hal, T: Transport> Device for VirtIoBlkDev<H, T> {
    fn name(&self) -> &str {
        &self.name
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }
}

impl<H: Hal, T: Transport> BlockDeviceOperations for VirtIoBlkDev<H, T> {
    #[inline]
    fn num_blocks(&self) -> u64 {
        self.num_blocks
    }

    #[inline]
    fn block_size(&self) -> usize {
        SECTOR_SIZE
    }

    fn is_inherently_read_only(&self) -> bool {
        self.is_read_only
    }

    fn read_block(&self, block_id: u64, buf: &mut [u8]) -> DriverResult {
        let _call = self.admit_call()?;
        if let Some(prepare) = self.prepare_wait {
            return self.read_request(block_id, buf, prepare);
        }
        self.state
            .lock()
            .device
            .as_mut()
            .expect("admitted block transport")
            .read_blocks(block_id as usize, buf)
            .map_err(as_driver_error)
    }

    fn write_block(&self, block_id: u64, buf: &[u8]) -> DriverResult {
        let _call = self.admit_call()?;
        if let Some(prepare) = self.prepare_wait {
            return self.write_request(block_id, buf, prepare);
        }
        let mut state = self.state.lock();
        if self.is_read_only {
            return Err(DriverError::ReadOnly);
        }
        state
            .device
            .as_mut()
            .expect("admitted block transport")
            .write_blocks(block_id as usize, buf)
            .map_err(as_driver_error)
    }

    fn flush(&self) -> DriverResult {
        let _call = self.admit_call()?;
        if let Some(prepare) = self.prepare_wait {
            return self.flush_request(prepare);
        }
        self.state
            .lock()
            .device
            .as_mut()
            .expect("admitted block transport")
            .flush()
            .map_err(as_driver_error)
    }
}

unittest::path_mod_test!(blk_unittest, "../ut-tests/blk_unittest.rs");
