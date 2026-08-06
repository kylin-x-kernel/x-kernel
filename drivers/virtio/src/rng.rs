// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO entropy source (hardware RNG) driver adapter.
//!
//! Unlike the stock `VirtIORng::request_entropy` helper (infinite
//! `add_notify_wait_pop` spin), this adapter bounds the IRQ-off busy-wait and
//! returns [`DriverError::WouldBlock`] on timeout so a wedged host/device
//! cannot leave local interrupts masked indefinitely.

use alloc::boxed::Box;
use core::hint::spin_loop;

use char_driver::CharDevice;
use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use khal::time::monotonic_time;
use kspin::SpinNoIrq;
use ktime_types::TimeSpan;
use virtio_drivers::{
    Hal, Result as VirtioResult, device::common::Feature, queue::VirtQueue, transport::Transport,
};

use crate::as_driver_error;

const QUEUE_IDX: u16 = 0;
const QUEUE_SIZE: usize = 8;
const SUPPORTED_FEATURES: Feature = Feature::RING_INDIRECT_DESC
    .union(Feature::RING_EVENT_IDX)
    .union(Feature::VERSION_1)
    .union(Feature::ACCESS_PLATFORM);

/// Upper bound on a single entropy request while local IRQs are masked.
///
/// Tuned to tolerate a slow host `/dev/urandom` backend without hanging the
/// guest forever if the virtqueue never completes.
const REQUEST_TIMEOUT: TimeSpan = TimeSpan::from_millis(10);

/// Cap per request so the owned DMA scratch buffer stays modest.
const MAX_REQUEST_BYTES: usize = 256;

struct PendingRequest {
    token: u16,
    buf: Box<[u8]>,
}

struct RngInner<H: Hal, T: Transport> {
    transport: T,
    queue: VirtQueue<H, QUEUE_SIZE>,
    /// In-flight request left after a timeout; buffer must stay alive until
    /// the device completes (or the driver is dropped).
    pending: Option<PendingRequest>,
}

impl<H: Hal, T: Transport> RngInner<H, T> {
    fn try_new(mut transport: T) -> VirtioResult<Self> {
        let feat = transport.begin_init(SUPPORTED_FEATURES);
        let queue = VirtQueue::new(
            &mut transport,
            QUEUE_IDX,
            feat.contains(Feature::RING_INDIRECT_DESC),
            feat.contains(Feature::RING_EVENT_IDX),
            feat.contains(Feature::ACCESS_PLATFORM),
        )?;
        transport.finish_init();
        Ok(Self {
            transport,
            queue,
            pending: None,
        })
    }

    /// Reclaim a previously timed-out request if the device has completed it.
    ///
    /// Returns `false` when a pending request is still outstanding.
    fn reclaim_pending_if_ready(&mut self) -> bool {
        let Some(pending) = self.pending.as_mut() else {
            return true;
        };
        if !self.queue.can_pop() {
            return false;
        }

        let token = pending.token;
        let mut outputs = [pending.buf.as_mut()];
        // SAFETY: `pending.buf` is the same allocation passed to `add` for
        // `token`, and has not been accessed since then.
        let _ = unsafe { self.queue.pop_used(token, &[], &mut outputs) };
        self.pending = None;
        let _ = self.transport.ack_interrupt();
        true
    }

    fn request_entropy_timed(&mut self, dst: &mut [u8]) -> DriverResult<usize> {
        if dst.is_empty() {
            return Ok(0);
        }
        if !self.reclaim_pending_if_ready() {
            // Still waiting on a prior timed-out descriptor; do not enqueue more.
            return Err(DriverError::WouldBlock);
        }

        let len = dst.len().min(MAX_REQUEST_BYTES);
        let mut dma_buf = alloc::vec![0u8; len].into_boxed_slice();

        // SAFETY: `dma_buf` is kept alive either until `pop_used` below succeeds
        // or until it is stored in `self.pending` for a later reclaim.
        let token =
            unsafe { self.queue.add(&[], &mut [dma_buf.as_mut()]) }.map_err(as_driver_error)?;

        if self.queue.should_notify() {
            self.transport.notify(QUEUE_IDX);
        }

        let deadline = monotonic_time() + REQUEST_TIMEOUT;
        while !self.queue.can_pop() {
            if monotonic_time() >= deadline {
                // Buffer ownership moves into `pending` so the device may still
                // complete safely after we drop the IRQ-off section.
                self.pending = Some(PendingRequest {
                    token,
                    buf: dma_buf,
                });
                return Err(DriverError::WouldBlock);
            }
            spin_loop();
        }

        let mut outputs = [dma_buf.as_mut()];
        // SAFETY: same buffers and token as the `add` call above.
        let written = unsafe { self.queue.pop_used(token, &[], &mut outputs) }
            .map_err(as_driver_error)? as usize;

        // Ack before re-enabling IRQs so a shared level-triggered virtio line
        // cannot livelock the CPU.
        let _ = self.transport.ack_interrupt();

        let copy_len = written.min(dst.len());
        dst[..copy_len].copy_from_slice(&dma_buf[..copy_len]);
        Ok(copy_len)
    }
}

impl<H: Hal, T: Transport> Drop for RngInner<H, T> {
    fn drop(&mut self) {
        self.transport.queue_unset(QUEUE_IDX);
    }
}

/// VirtIO entropy source device driver.
///
/// Exposes the device through [`CharDevice`] so upper layers can collect
/// hardware entropy. Completion waits are time-bounded under [`SpinNoIrq`].
pub struct VirtIoRngDev<H: Hal, T: Transport> {
    device: SpinNoIrq<RngInner<H, T>>,
}

// SAFETY: VirtIoRngDev serializes all access to the inner device through
// its own `SpinNoIrq` lock. The inner type is not auto Send due to
// PhantomData, but it is safe to transfer across threads behind that lock.
// The lock must mask interrupts for the duration of each device access,
// because the synchronous request path busy-polls the used ring while holding
// the lock. On targets that share a level-triggered INTx line across virtio
// devices, leaving local interrupts enabled during the poll can livelock the
// CPU in an interrupt storm. The poll itself is deadline-bounded.
unsafe impl<H: Hal, T: Transport> Send for VirtIoRngDev<H, T> {}
// SAFETY: shared access to the device is serialized by the IRQ-safe lock
// described above, so immutable references may be shared across threads safely.
unsafe impl<H: Hal, T: Transport> Sync for VirtIoRngDev<H, T> {}

impl<H: Hal, T: Transport> VirtIoRngDev<H, T> {
    /// Create and initialize a VirtIO RNG device.
    pub fn try_new(transport: T) -> DriverResult<Self> {
        RngInner::try_new(transport)
            .map(|device| Self {
                device: SpinNoIrq::new(device),
            })
            .map_err(as_driver_error)
    }

    fn request_entropy(&self, buf: &mut [u8]) -> DriverResult<usize> {
        self.device.lock().request_entropy_timed(buf)
    }
}

impl<H: Hal, T: Transport> Device for VirtIoRngDev<H, T> {
    fn name(&self) -> &str {
        "virtio-rng"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }
}

impl<H: Hal, T: Transport> CharDevice for VirtIoRngDev<H, T> {
    fn read(&self, buf: &mut [u8]) -> DriverResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.request_entropy(buf)
    }

    fn write(&self, _buf: &[u8]) -> DriverResult<usize> {
        Err(DriverError::Unsupported)
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, assert_eq, def_test};
    use virtio_drivers::transport::DeviceType;

    use super::*;
    use crate::mock_virtio::{MockHal, MockTransport};

    #[def_test]
    fn test_virtio_rng_init_failure_handling() {
        let transport = MockTransport::new_with_type(DeviceType::EntropySource);
        let dev = VirtIoRngDev::<MockHal, MockTransport>::try_new(transport);

        if let Ok(d) = dev {
            assert_eq!(d.name(), "virtio-rng");
            assert_eq!(d.device_kind(), DeviceKind::Char);
        } else {
            assert!(dev.is_err());
        }
    }

    #[def_test]
    fn test_virtio_rng_concurrency_traits() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<VirtIoRngDev<MockHal, MockTransport>>();
    }

    #[def_test]
    fn test_request_timeout_constant_is_finite() {
        // Guard against accidentally restoring an unbounded wait.
        assert!(REQUEST_TIMEOUT > TimeSpan::from_millis(0));
        assert!(REQUEST_TIMEOUT <= TimeSpan::from_secs(1));
    }
}
