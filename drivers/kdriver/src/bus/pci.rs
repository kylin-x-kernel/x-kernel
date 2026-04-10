// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! PCI bus probing and BAR configuration.
use pci::{
    Cam, HeaderType, PciBus, PciConfigSource, PciRangeAllocator, configure_device,
    pci_bar_allocation_range,
};

use crate::{AllDevices, prelude::*};

impl AllDevices {
    /// Enumerate PCI devices and register matching drivers.
    pub(crate) fn probe_bus_devices(&mut self) {
        #[cfg(feature = "pci-mmio")]
        let cam_kind = Cam::MmioCam;
        #[cfg(not(feature = "pci-mmio"))]
        let cam_kind = Cam::Ecam;
        let mut bus = match PciBus::new(cam_kind) {
            Ok(bus) => bus,
            Err(err) => {
                error!("failed to initialize PCI bus: {:?}", err);
                return;
            }
        };
        info!(
            "PCI config space: source={}, ecam={:#x}, bus_end={:#x}",
            match bus.source() {
                PciConfigSource::RuntimeOverride => "runtime-override",
                PciConfigSource::DeviceTree => "device-tree",
                PciConfigSource::Static => "static",
            },
            bus.config_base(),
            bus.bus_end()
        );

        // PCI 32-bit MMIO space
        let mut allocator =
            pci_bar_allocation_range().map(|(start, len, _)| PciRangeAllocator::new(start, len));

        let pci_bus_end = bus.bus_end();
        let (root, config) = bus.parts_mut();
        for bus in 0..=pci_bus_end {
            for (bdf, dev_info) in root.enumerate_bus(bus) {
                debug!("PCI {bdf}: {dev_info}");
                if dev_info.header_type != HeaderType::Standard {
                    continue;
                }
                match configure_device(root, bdf, &mut allocator) {
                    Ok(_) => for_each_drivers!(type Driver, {
                        if let Some(dev) = Driver::probe_pci(root, config, bdf, &dev_info) {
                            info!(
                                "registered a new {:?} device at {}: {:?}",
                                dev.device_kind(),
                                bdf,
                                dev.name(),
                            );
                            self.add_device(dev);
                            continue; // skip to the next device
                        }
                    }),
                    Err(e) => warn!("failed to enable PCI device at {bdf}({dev_info}): {:?}", e),
                }
            }
        }
    }
}
