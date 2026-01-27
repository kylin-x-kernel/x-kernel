use alloc::{borrow::ToOwned, string::String};

use driver_base::{DeviceKind, DriverError, DriverOps, DriverResult};
use input::{Event, EventType, InputDeviceId, InputDriverOps};
use virtio_drivers::{
    Hal,
    device::input::{InputConfigSelect, VirtIOInput as InnerDev},
    transport::Transport,
};

use crate::as_driver_error;

/// The VirtIO Input device driver.
pub struct VirtIoInputDev<H: Hal, T: Transport> {
    inner: InnerDev<H, T>,
    device_id: InputDeviceId,
    name: String,
}

unsafe impl<H: Hal, T: Transport> Send for VirtIoInputDev<H, T> {}
unsafe impl<H: Hal, T: Transport> Sync for VirtIoInputDev<H, T> {}

impl<H: Hal, T: Transport> VirtIoInputDev<H, T> {
    /// Creates a new driver instance and initializes the device, or returns
    /// an error if any step fails.
    pub fn try_new(transport: T) -> DriverResult<Self> {
        let mut virtio = InnerDev::new(transport).unwrap();
        let name = virtio.name().unwrap_or_else(|_| "<unknown>".to_owned());
        let device_id = virtio.ids().map_err(as_driver_error)?;
        let device_id = InputDeviceId {
            bus_type: device_id.bustype,
            vendor: device_id.vendor,
            product: device_id.product,
            version: device_id.version,
        };

        Ok(Self {
            inner: virtio,
            device_id,
            name,
        })
    }
}

impl<H: Hal, T: Transport> DriverOps for VirtIoInputDev<H, T> {
    fn name(&self) -> &str {
        &self.name
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
        // TODO: unique physical location
        "virtio0/input0"
    }

    fn unique_id(&self) -> &str {
        // TODO: unique ID
        "virtio"
    }

    fn get_event_bits(&mut self, ty: EventType, out: &mut [u8]) -> DriverResult<bool> {
        let read = self
            .inner
            .query_config_select(InputConfigSelect::EvBits, ty as u8, out);
        Ok(read != 0)
    }

    fn read_event(&mut self) -> DriverResult<Event> {
        self.inner.ack_interrupt();
        self.inner
            .pop_pending_event()
            .map(|e| Event {
                event_type: e.event_type,
                code: e.code,
                value: e.value,
            })
            .ok_or(DriverError::WouldBlock)
    }
}
