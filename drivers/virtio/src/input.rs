// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO input driver adapter.
use alloc::string::String;

use driver_base::{DeviceKind, DriverError, DriverOps, DriverResult};
use input::{Event, EventType, InputDeviceId, InputDriverOps};
use virtio_drivers::{
    Hal,
    device::input::{InputConfigSelect, VirtIOInput as InnerDev},
    transport::Transport,
};

use crate::as_driver_error;

/// The VirtIO Input device driver.
///
/// Wraps [`VirtIOInput`] from `virtio-drivers` and implements the
/// [`InputDriverOps`] trait, providing event reading and capability query
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
/// let mut input = VirtIoInputDev::<HalImpl, _>::try_new(transport)?;
/// let event = input.read_event()?;
/// ```
pub struct VirtIoInputDev<H: Hal, T: Transport> {
    inner: InnerDev<H, T>,
    device_id: InputDeviceId,
    name: String,
}

// SAFETY: VirtIoInputDev accesses the device exclusively through &mut self.
// The inner VirtIOInput is not auto Send/Sync due to PhantomData, but it is
// safe to transfer across threads and share immutable references.
unsafe impl<H: Hal, T: Transport> Send for VirtIoInputDev<H, T> {}
unsafe impl<H: Hal, T: Transport> Sync for VirtIoInputDev<H, T> {}

impl<H: Hal, T: Transport> VirtIoInputDev<H, T> {
    /// Creates a new driver instance, reads the device name and IDs, and
    /// initializes the VirtIO input device.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError`] if device initialization or ID query fails.
    pub fn try_new(transport: T) -> DriverResult<Self> {
        let mut device = InnerDev::new(transport).map_err(as_driver_error)?;
        let name = device.name().unwrap_or_else(|_| String::from("<unknown>"));
        let ids = device.ids().map_err(as_driver_error)?;

        Ok(Self {
            inner: device,
            device_id: InputDeviceId {
                bus_type: ids.bustype,
                vendor: ids.vendor,
                product: ids.product,
                version: ids.version,
            },
            name,
        })
    }

    fn load_event_bits(&mut self, event_type: EventType, out: &mut [u8]) -> DriverResult<bool> {
        let written = self
            .inner
            .query_config_select(InputConfigSelect::EvBits, event_type as u8, out)
            .map_err(as_driver_error)?;
        Ok(written != 0)
    }
}

impl<H: Hal, T: Transport> DriverOps for VirtIoInputDev<H, T> {
    fn name(&self) -> &str {
        self.name.as_str()
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Input
    }
}

impl<H: Hal, T: Transport> InputDriverOps for VirtIoInputDev<H, T> {
    fn device_id(&self) -> InputDeviceId {
        self.device_id
    }

    fn physical_location(&self) -> &str {
        "virtio0/input0"
    }

    fn unique_id(&self) -> &str {
        "virtio"
    }

    fn get_event_bits(&mut self, ty: EventType, out: &mut [u8]) -> DriverResult<bool> {
        self.load_event_bits(ty, out)
    }

    fn read_event(&mut self) -> DriverResult<Event> {
        let Some(event) = self.inner.pop_pending_event() else {
            return Err(DriverError::WouldBlock);
        };
        Ok(Event {
            event_type: event.event_type,
            code: event.code,
            value: event.value,
        })
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, def_test};

    use super::*;
    use crate::mock_virtio::{MockHal, MockTransport};

    #[def_test]
    fn test_virtio_input_init_failure() {
        let mut transport = MockTransport::new();
        transport.device_type = virtio_drivers::transport::DeviceType::Input;
        let dev = VirtIoInputDev::<MockHal, MockTransport>::try_new(transport);
        assert!(dev.is_err());
    }

    #[def_test]
    fn test_virtio_input_traits() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<VirtIoInputDev<MockHal, MockTransport>>();
    }
}
