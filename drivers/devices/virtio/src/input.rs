// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO input driver adapter.
use alloc::{string::String, sync::Arc};
use core::{
    mem::size_of,
    sync::atomic::{AtomicBool, Ordering},
};

use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use driver_os_api::{IrqEvent, IrqHandler};
use input::{
    ABS_BITMAP_LEN, ABS_CNT, AbsInfo, Event, EventType, InputDevice, InputDeviceId,
    InputEventNotifier, PROP_BITMAP_LEN,
};
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
    /// Sticky conservative readiness hint. It starts true so events that
    /// predate the first read cannot be hidden by an idle check.
    is_readable: AtomicBool,
    notifier: SpinNoIrq<Option<Arc<dyn InputEventNotifier>>>,
    /// Immutable after probing; the input core owns live axis values.
    abs_info: [Option<AbsInfo>; ABS_CNT],
    prop_bits: [u8; PROP_BITMAP_LEN],
    has_prop: bool,
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
            // Advisory hint only: this publishes no queue data, and the queue
            // itself is synchronized by the `inner` lock. Relaxed is sufficient.
            self.state.is_readable.store(true, Ordering::Relaxed);
            let notifier = self.state.notifier.lock().clone();
            if let Some(notifier) = notifier {
                notifier.notify_events();
            }
        }
        IrqEvent::HANDLED
    }
}

fn parse_abs_info(raw: &[u8; 5 * size_of::<u32>()]) -> AbsInfo {
    let mut fields = [0i32; 5];
    for (field, word) in fields.iter_mut().zip(raw.chunks_exact(size_of::<i32>())) {
        *field = i32::from_le_bytes([word[0], word[1], word[2], word[3]]);
    }
    let [min, max, fuzz, flat, res] = fields;
    AbsInfo::new(min, max, fuzz, flat, res)
}

/// Probes the axes advertised by `EV_ABS` and caches their static metadata.
///
/// Config-space reads are best-effort: a device that does not answer a
/// capability query still probes, and the evdev layer then exposes the Linux
/// zero-filled view of that axis. The one-time cost also removes a config
/// transaction from every subsequent `EVIOCGABS`.
fn load_abs_info<H: Hal, T: Transport>(device: &mut InnerDev<H, T>) -> [Option<AbsInfo>; ABS_CNT] {
    let mut abs_info = [None; ABS_CNT];
    let mut bits = [0u8; ABS_BITMAP_LEN];
    let Ok(written) = device.query_config_select(
        InputConfigSelect::EvBits,
        EventType::Absolute as u8,
        &mut bits,
    ) else {
        return abs_info;
    };
    if written == 0 {
        return abs_info;
    }

    for (code, info) in abs_info.iter_mut().enumerate() {
        if bits[code / u8::BITS as usize] & (1 << (code % u8::BITS as usize)) == 0 {
            continue;
        }
        let mut raw = [0u8; 5 * size_of::<i32>()];
        let Ok(written) =
            device.query_config_select(InputConfigSelect::AbsInfo, code as u8, &mut raw)
        else {
            continue;
        };
        if usize::from(written) == raw.len() {
            *info = Some(parse_abs_info(&raw));
        }
    }
    abs_info
}

/// Probes the `INPUT_PROP_*` bitmap once at device initialization.
fn load_prop_bits<H: Hal, T: Transport>(
    device: &mut InnerDev<H, T>,
) -> Option<([u8; PROP_BITMAP_LEN], bool)> {
    let mut bits = [0u8; PROP_BITMAP_LEN];
    let written = device
        .query_config_select(InputConfigSelect::PropBits, 0, &mut bits)
        .ok()?;
    Some((bits, written != 0))
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
        let abs_info = load_abs_info(&mut device);
        let (prop_bits, has_prop) =
            load_prop_bits(&mut device).unwrap_or(([0u8; PROP_BITMAP_LEN], false));

        Ok(Self {
            state: Arc::new(VirtIoInputState {
                inner: SpinNoIrq::new(device),
                is_readable: AtomicBool::new(true),
                notifier: SpinNoIrq::new(None),
                abs_info,
                prop_bits,
                has_prop,
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

    fn abs_info(&self, code: u16) -> DriverResult<Option<AbsInfo>> {
        Ok(self
            .state
            .abs_info
            .get(usize::from(code))
            .copied()
            .flatten())
    }

    fn get_prop_bits(&self, out: &mut [u8]) -> DriverResult<bool> {
        let len = out.len().min(PROP_BITMAP_LEN);
        out[..len].copy_from_slice(&self.state.prop_bits[..len]);
        Ok(self.state.has_prop)
    }

    fn read_event(&self) -> DriverResult<Event> {
        // Keep the queue lock across the hint update: otherwise an interrupt
        // could enqueue an event and set the hint true between the pop and a
        // stale false store, hiding the new event from the next poll.
        let mut inner = self.state.inner.lock();
        let event = inner.pop_pending_event();
        // Advisory hint only; queue contents are synchronized by `inner`, so
        // no data is published through this atomic ordering.
        self.state
            .is_readable
            .store(event.is_some(), Ordering::Relaxed);
        drop(inner);
        let Some(event) = event else {
            return Err(DriverError::WouldBlock);
        };
        Ok(Event {
            event_type: event.event_type,
            code: event.code,
            value: event.value,
        })
    }

    fn poll_event_ready(&self) -> Option<bool> {
        // Never read/clear the transport ISR here. That read is destructive and
        // would race the hard-IRQ handler, swallowing the notification that
        // schedules the input endpoint drain. The IRQ handler owns that
        // acknowledgement and updates the same advisory hint.
        Some(self.state.is_readable.load(Ordering::Relaxed))
    }

    fn set_event_notifier(&self, notifier: Option<Arc<dyn InputEventNotifier>>) -> DriverResult {
        *self.state.notifier.lock() = notifier;
        Ok(())
    }
}

unittest::path_mod_test!(input_unittest, "../ut-tests/input_unittest.rs");
