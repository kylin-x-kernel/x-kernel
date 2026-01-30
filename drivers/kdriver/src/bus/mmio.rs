//! MMIO bus probing for virtio and other devices.
#[allow(unused_imports)]
use crate::{AllDevices, prelude::*};

impl AllDevices {
    /// Probe all MMIO device ranges defined by the platform config.
    pub(crate) fn probe_bus_devices(&mut self) {
        // TODO: parse device tree
        #[cfg(feature = "virtio")]
        for reg in platconfig::devices::VIRTIO_MMIO_RANGES {
            for_each_drivers!(type Driver, {
                if let Some(dev) = Driver::probe_mmio(reg.0, reg.1) {
                    info!(
                        "registered a new {:?} device at [PA:{:#x}, PA:{:#x}): {:?}",
                        dev.device_kind(),
                        reg.0, reg.0 + reg.1,
                        dev.name(),
                    );
                    self.add_device(dev);
                    continue; // skip to the next device
                }
            });
        }
    }
}
