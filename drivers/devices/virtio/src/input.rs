// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO input driver adapter.
use alloc::{string::String, sync::Arc};

use device_res::{IrqEvent, IrqHandler};
use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use input::{Event, EventType, InputDevice, InputDeviceId, InputEventNotifier};
use kspin::SpinNoIrq;
use virtio_drivers::{
    Hal,
    device::input::{InputConfigSelect, VirtIOInput as InnerDev},
    transport::{InterruptStatus, Transport},
};

use crate::as_driver_error;

/// The VirtIO Input device driver.
///
/// Wraps `VirtIOInput` from `virtio-drivers` and implements the
/// [`InputDevice`] trait, providing event reading and capability query
/// for virtual input devices (keyboard, mouse, etc.).
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
/// let input = VirtIoInputDev::<HalImpl, _>::try_new(transport, irq)?;
/// let handler = input.irq_handler();
/// let event = input.read_event()?;
/// ```
pub struct VirtIoInputDev<H: Hal, T: Transport> {
    state: Arc<VirtIoInputState<H, T>>,
    device_id: InputDeviceId,
    name: String,
    irq: usize,
}

struct VirtIoInputState<H: Hal, T: Transport> {
    inner: SpinNoIrq<InnerDev<H, T>>,
    notifier: SpinNoIrq<Option<Arc<dyn InputEventNotifier>>>,
}

// SAFETY: moving the state does not move its DMA allocations or MMIO regions,
// and the queue pointers continue to target those stable allocations. The
// transport has no CPU-local ownership, and access after the move still goes
// through `SpinNoIrq`.
unsafe impl<H: Hal, T: Transport> Send for VirtIoInputState<H, T> {}
// SAFETY: shared references cannot access `InnerDev` without taking the same
// IRQ-disabling spin lock, including from the hard-IRQ acknowledgement path.
unsafe impl<H: Hal, T: Transport> Sync for VirtIoInputState<H, T> {}

struct VirtIoInputIrqHandler<H: Hal, T: Transport> {
    irq: usize,
    state: Arc<VirtIoInputState<H, T>>,
}

impl<H: Hal + 'static, T: Transport + 'static> IrqHandler for VirtIoInputIrqHandler<H, T> {
    fn handle(&self, irq: usize) -> IrqEvent {
        if irq != self.irq {
            return IrqEvent::NOT_HANDLED;
        }
        let status = self.state.inner.lock().ack_interrupt();
        if status.is_empty() {
            return IrqEvent::NOT_HANDLED;
        }
        if status.contains(InterruptStatus::QUEUE_INTERRUPT) {
            let notifier = self.state.notifier.lock().clone();
            if let Some(notifier) = notifier {
                notifier.notify_events();
            }
        }
        IrqEvent::HANDLED
    }
}

impl<H: Hal, T: Transport> VirtIoInputDev<H, T> {
    /// Creates an IRQ-backed driver instance and reads its identity.
    ///
    /// # Errors
    ///
    /// Returns `DriverError` if device initialization or ID query fails.
    pub fn try_new(transport: T, irq: usize) -> DriverResult<Self> {
        let mut device = InnerDev::new(transport).map_err(as_driver_error)?;
        let name = device.name().unwrap_or_else(|_| String::from("<unknown>"));
        let name = String::from(name.trim_end_matches('\0'));
        let ids = device.ids().map_err(as_driver_error)?;

        Ok(Self {
            state: Arc::new(VirtIoInputState {
                inner: SpinNoIrq::new(device),
                notifier: SpinNoIrq::new(None),
            }),
            device_id: InputDeviceId {
                bus_type: ids.bustype,
                vendor: ids.vendor,
                product: ids.product,
                version: ids.version,
            },
            name,
            irq,
        })
    }

    /// Returns a hard-IRQ handler that acknowledges this device's transport.
    ///
    /// The handler shares the transport lock with task-context event reads. It
    /// does not consume input events or allocate while handling an interrupt.
    pub fn irq_handler(&self) -> Arc<dyn IrqHandler>
    where
        H: 'static,
        T: 'static,
    {
        Arc::new(VirtIoInputIrqHandler {
            irq: self.irq,
            state: self.state.clone(),
        })
    }

    fn load_event_bits(&self, event_type: EventType, out: &mut [u8]) -> DriverResult<bool> {
        let written = self
            .state
            .inner
            .lock()
            .query_config_select(InputConfigSelect::EvBits, event_type as u8, out)
            .map_err(as_driver_error)?;
        Ok(written != 0)
    }
}

impl<H: Hal, T: Transport> Device for VirtIoInputDev<H, T> {
    fn name(&self) -> &str {
        self.name.as_str()
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Input
    }

    fn irq(&self) -> Option<usize> {
        Some(self.irq)
    }
}

impl<H: Hal, T: Transport> InputDevice for VirtIoInputDev<H, T> {
    fn device_id(&self) -> InputDeviceId {
        self.device_id
    }

    fn physical_location(&self) -> &str {
        "virtio0/input0"
    }

    fn unique_id(&self) -> &str {
        "virtio"
    }

    fn get_event_bits(&self, ty: EventType, out: &mut [u8]) -> DriverResult<bool> {
        self.load_event_bits(ty, out)
    }

    fn read_event(&self) -> DriverResult<Event> {
        let Some(event) = self.state.inner.lock().pop_pending_event() else {
            return Err(DriverError::WouldBlock);
        };
        Ok(Event {
            event_type: event.event_type,
            code: event.code,
            value: event.value,
        })
    }

    fn set_event_notifier(&self, notifier: Option<Arc<dyn InputEventNotifier>>) -> DriverResult {
        *self.state.notifier.lock() = notifier;
        Ok(())
    }
}

unittest::path_mod_test!(input_unittest, "../ut-tests/input_unittest.rs");
