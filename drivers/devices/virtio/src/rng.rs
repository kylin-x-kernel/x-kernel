// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Interrupt-driven VirtIO RNG adapter.

use alloc::{boxed::Box, sync::Arc, vec};
use core::{mem::ManuallyDrop, ops::Range};

use device_res::{Irq, IrqEvent, IrqHandler, IrqOp, IrqResource, ResError};
use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use hwrng::{HwrngDevice, completion::RngCompletion};
use kspin::SpinNoIrq;
use virtio_drivers::{
    Hal,
    device::rng::VirtIORng,
    transport::{InterruptStatus, Transport},
};

use crate::as_driver_error;

const MAX_REQUEST_BYTES: usize = 256;

fn map_irq_error(error: ResError) -> DriverError {
    match error {
        ResError::InvalidResource => DriverError::InvalidInput,
        ResError::MappingFailed => DriverError::Io,
        ResError::NoMemory => DriverError::NoMemory,
        ResError::Busy => DriverError::ResourceBusy,
        ResError::Unsupported => DriverError::Unsupported,
    }
}

enum RequestState {
    Idle,
    Pending(u16),
    Readable(Range<usize>),
}

/// Queue state and DMA storage that a submitted request must retain together.
struct RequestResources<H: Hal, T: Transport> {
    device: VirtIORng<H, T>,
    buffer: Box<[u8]>,
}

struct RngRequest<H: Hal, T: Transport> {
    resources: ManuallyDrop<RequestResources<H, T>>,
    state: RequestState,
    fault: Option<DriverError>,
}

// SAFETY: H supplies kernel-global mappings through dma_alloc and
// mmio_phys_to_virt: queue/DMA pointers remain valid across threads and CPUs,
// rather than being private to the creating thread or CPU. The queue, its state,
// and the owned Box<[u8]> move together without relocating their backing storage.
// H: Send and T: Send also require the HAL and transport themselves to be
// transferable; moving the request therefore preserves its pointer validity.
unsafe impl<H: Hal + Send, T: Transport + Send> Send for RngRequest<H, T> {}

impl<H: Hal, T: Transport> RngRequest<H, T> {
    fn new(transport: T) -> DriverResult<Self> {
        let buffer = vec![0; MAX_REQUEST_BYTES].into_boxed_slice();
        let device = VirtIORng::new(transport).map_err(as_driver_error)?;
        Ok(Self {
            resources: ManuallyDrop::new(RequestResources { device, buffer }),
            state: RequestState::Idle,
            fault: None,
        })
    }

    /// Only the waiting-read path submits; cached or in-flight data is preserved.
    fn submit(&mut self) -> DriverResult {
        if let Some(error) = self.fault {
            return Err(error);
        }
        // Cached bytes share the DMA buffer; do not resubmit until they are fully consumed.
        if !matches!(self.state, RequestState::Idle) {
            return Ok(());
        }
        let RequestResources { device, buffer } = &mut *self.resources;
        // SAFETY: the private Box has a stable address/length. Pending prevents
        // CPU reads and resubmission; Drop retains both resources if still live.
        match unsafe { device.request_entropy_nb(buffer) } {
            Ok(token) => {
                self.state = RequestState::Pending(token);
                Ok(())
            }
            Err(error) => {
                // A fixed nonempty buffer and one outstanding request cannot
                // exhaust this queue. These errors indicate broken invariants.
                if matches!(
                    error,
                    virtio_drivers::Error::QueueFull | virtio_drivers::Error::InvalidParam
                ) {
                    self.fault = Some(as_driver_error(error));
                }
                Err(as_driver_error(error))
            }
        }
    }

    /// Attempts one reclamation. The IRQ adapter, never a reader, calls this.
    /// Returns true only when an outstanding request was actually reclaimed.
    fn complete(&mut self) -> DriverResult<bool> {
        let RequestState::Pending(token) = self.state else {
            return Ok(false);
        };
        let RequestResources { device, buffer } = &mut *self.resources;
        // SAFETY: Pending holds the original live token; buffer is the original
        // Box at its full submitted length. Exclusive self serializes reclaim.
        let len = match unsafe { device.complete_request_entropy(token, buffer) } {
            Ok(len) => len,
            // A notification is only a hint; ownership stays with the device
            // until a matching used entry arrives in a later completion IRQ.
            Err(virtio_drivers::Error::NotReady) => return Ok(false),
            Err(error) => {
                let error = as_driver_error(error);
                self.fault.get_or_insert(error);
                return Err(error);
            }
        };
        self.state = RequestState::Idle;
        if len == 0 || len > MAX_REQUEST_BYTES {
            return Ok(true);
        }
        if self.fault.is_none() {
            self.state = RequestState::Readable(0..len);
        }
        Ok(true)
    }

    /// Consumes cached bytes only, including retaining a short read's remainder.
    fn read(&mut self, dst: &mut [u8]) -> usize {
        if let Some(_error) = self.fault {
            return 0;
        }
        let RequestState::Readable(remaining) = &mut self.state else {
            return 0;
        };

        let count = dst.len().min(remaining.len());
        dst[..count]
            .copy_from_slice(&self.resources.buffer[remaining.start..remaining.start + count]);
        remaining.start += count;
        if Range::is_empty(remaining) {
            self.state = RequestState::Idle;
        }
        count
    }
}

impl<H: Hal, T: Transport> Drop for RngRequest<H, T> {
    fn drop(&mut self) {
        if matches!(self.state, RequestState::Pending(_)) {
            // Emergency fallback for a lost owner, which
            // must not free DMA memory, even when completion was malformed.
            log::error!("virtio-rng: lost request owner; retaining live DMA resources");
            return;
        }
        // SAFETY: no outstanding submission can access these request resources.
        // They are dropped exactly once here; active transport lifetime is a host duty.
        unsafe { ManuallyDrop::drop(&mut self.resources) };
    }
}

/// IRQ-driven hardware entropy source using a driver-owned DMA buffer.
///
/// While the published instance exists, it owns its IRQ registration and mappings.
/// Device removal and rebinding are not supported.
/// Completion reclaims at most one request inline in hard IRQ context, including
/// HAL unshare, which must be IRQ-safe and non-sleeping. This limits the amount
/// of request work, not lock-wait latency: the X-Kernel HAL unmaps through the
/// global kdma allocator lock, shared with other DMA operations. No worst-case
/// IRQ latency bound has been established for that backend.
/// HAL DMA and MMIO mappings must be kernel-global, independent of the creating
/// thread or CPU, so request ownership can move across threads.
/// The `request` SpinNoIrq lock serializes readers and IRQ completion, providing
/// exclusive access to the queue, buffer, and request state through shared device
/// references; RngRequest itself does not implement Sync.
pub struct VirtIoRngDev<H: Hal, T: Transport> {
    request: SpinNoIrq<RngRequest<H, T>>,
    completion: Arc<dyn RngCompletion>,
    // Drop after `request`, so a normal request cleanup runs before this guard
    // unregisters its completion handler.
    irq_guard: SpinNoIrq<Option<Irq>>,
}

impl<H: Hal + Send + 'static, T: Transport + Send + 'static> VirtIoRngDev<H, T> {
    /// Register completion delivery before allowing reads.
    ///
    /// The returned device owns the IRQ registration. Hardware teardown and
    /// rebinding are not supported by this adapter yet.
    pub fn try_new_irq(
        transport: T,
        completion: Arc<dyn RngCompletion>,
        provider: &'static dyn IrqOp,
        resource: IrqResource,
    ) -> DriverResult<Arc<Self>> {
        let device = Arc::new(Self::new(transport, completion)?);
        let handler: Arc<dyn IrqHandler> = device.clone();
        let weak = Arc::downgrade(&handler);
        let irq = Irq::request_with(
            provider,
            resource,
            Arc::new(move |number| {
                weak.upgrade()
                    .map_or(IrqEvent::NOT_HANDLED, |device| device.handle(number))
            }),
        )
        .map_err(map_irq_error)?;
        *device.irq_guard.lock() = Some(irq);
        device.request.lock().resources.device.enable_interrupts();
        Ok(device)
    }
}

impl<H: Hal, T: Transport> VirtIoRngDev<H, T> {
    // Only try_new_irq publishes this object; tests drive its IRQ explicitly.
    fn new(transport: T, completion: Arc<dyn RngCompletion>) -> DriverResult<Self> {
        Ok(Self {
            request: SpinNoIrq::new(RngRequest::new(transport)?),
            completion,
            irq_guard: SpinNoIrq::new(None),
        })
    }
}

impl<H: Hal + Send, T: Transport + Send> Device for VirtIoRngDev<H, T> {
    fn name(&self) -> &str {
        "virtio-rng"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Hwrng
    }
}

impl<H: Hal + Send, T: Transport + Send> HwrngDevice for VirtIoRngDev<H, T> {
    fn is_failed(&self) -> bool {
        self.request.lock().fault.is_some()
    }

    fn read(&self, buf: &mut [u8]) -> DriverResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut result = Ok(0);
        self.completion.wait_until(&mut || {
            let mut request = self.request.lock();
            let count = request.read(buf);
            if count > 0 {
                result = Ok(count);
                return true;
            }

            if let Err(error) = request.submit() {
                result = Err(error);
                drop(request);
                self.completion.notify();
                return true;
            }
            false
        })?;
        result
    }
}

impl<H: Hal + Send, T: Transport + Send> IrqHandler for VirtIoRngDev<H, T> {
    fn handle(&self, _irq: usize) -> IrqEvent {
        let mut request = self.request.lock();
        let status = request.resources.device.ack_interrupt();
        if status.is_empty() {
            return IrqEvent::NOT_HANDLED;
        }
        if !status.contains(InterruptStatus::QUEUE_INTERRUPT) {
            return IrqEvent::HANDLED;
        }
        let completed = request.complete();
        let has_fault = request.fault.is_some();
        drop(request);
        if has_fault || completed == Ok(true) {
            self.completion.notify();
        }
        IrqEvent::HANDLED
    }
}

unittest::path_mod_test!(rng_unittest, "../ut-tests/rng_unittest.rs");
