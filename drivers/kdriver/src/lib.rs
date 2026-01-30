// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! [x-kernel] device drivers.
//!
//! All detected devices are composed into [`AllDevices`] and returned by [`init_drivers`].
//!
//! Device categories: [`NetDevice`], [`BlockDevice`], [`DisplayDevice`].
//!
//! Supports static and dynamic device models via the `dyn` feature.

#![no_std]
#![feature(doc_cfg)]
#![feature(associated_type_defaults)]

#[macro_use]
extern crate log;

#[macro_use]
mod macros;

mod bus;
mod drivers;
mod dummy;
mod structs;

#[cfg(feature = "virtio")]
mod virtio;

// #[cfg(feature = "ixgbe")]
// mod ixgbe;

pub mod prelude;

#[allow(unused_imports)]
use self::prelude::*;
#[cfg(feature = "block")]
pub use self::structs::BlockDevice;
#[cfg(feature = "display")]
pub use self::structs::DisplayDevice;
#[cfg(feature = "net")]
pub use self::structs::NetDevice;
pub use self::structs::{DeviceContainer, DeviceEnum};

/// A structure that contains all device drivers, organized by their category.
#[derive(Default)]
pub struct AllDevices {
    /// All network device drivers.
    #[cfg(feature = "net")]
    pub net: DeviceContainer<NetDevice>,
    /// All block device drivers.
    #[cfg(feature = "block")]
    pub block: DeviceContainer<BlockDevice>,
    /// All graphics device drivers.
    #[cfg(feature = "display")]
    pub display: DeviceContainer<DisplayDevice>,
    /// All input device drivers.
    #[cfg(feature = "input")]
    pub input: DeviceContainer<InputDevice>,
    /// All vsock device drivers.
    #[cfg(feature = "vsock")]
    pub vsock: DeviceContainer<VsockDevice>,
}

impl AllDevices {
    /// Returns the device model used.
    pub const fn device_model() -> &'static str {
        "static"
    }

    /// Probes all supported devices.
    fn probe(&mut self) {
        for_each_drivers!(type Driver, {
            if let Some(dev) = Driver::probe_global() {
                info!(
                    "registered a new {:?} device: {:?}",
                    dev.device_kind(),
                    dev.name(),
                );
                self.add_device(dev);
            }
        });
        self.probe_bus_devices();
    }

    /// Adds device to corresponding container.
    #[allow(dead_code)]
    fn add_device(&mut self, dev: DeviceEnum) {
        match dev {
            #[cfg(feature = "net")]
            DeviceEnum::Net(dev) => self.net.push(dev),
            #[cfg(feature = "block")]
            DeviceEnum::Block(dev) => self.block.push(dev),
            #[cfg(feature = "display")]
            DeviceEnum::Display(dev) => self.display.push(dev),
            #[cfg(feature = "input")]
            DeviceEnum::Input(dev) => self.input.push(dev),
            #[cfg(feature = "vsock")]
            DeviceEnum::Vsock(dev) => self.vsock.push(dev),
        }
    }
}

/// Initializes all device drivers.
pub fn init_drivers() -> AllDevices {
    info!("Initialize device drivers...");
    info!("  device model: {}", AllDevices::device_model());

    let mut all_devs = AllDevices::default();
    all_devs.probe();

    #[cfg(feature = "net")]
    {
        debug!("number of NICs: {}", all_devs.net.len());
        for (i, dev) in all_devs.net.iter().enumerate() {
            assert_eq!(dev.device_kind(), DeviceKind::Net);
            debug!("  NIC {}: {:?}", i, dev.name());
        }
    }
    #[cfg(feature = "block")]
    {
        debug!("number of block devices: {}", all_devs.block.len());
        for (i, dev) in all_devs.block.iter().enumerate() {
            assert_eq!(dev.device_kind(), DeviceKind::Block);
            debug!("  block device {}: {:?}", i, dev.name());
        }
    }
    #[cfg(feature = "display")]
    {
        debug!("number of graphics devices: {}", all_devs.display.len());
        for (i, dev) in all_devs.display.iter().enumerate() {
            assert_eq!(dev.device_kind(), DeviceKind::Display);
            debug!("  graphics device {}: {:?}", i, dev.name());
        }
    }
    #[cfg(feature = "input")]
    {
        debug!("number of input devices: {}", all_devs.input.len());
        for (i, dev) in all_devs.input.iter().enumerate() {
            assert_eq!(dev.device_kind(), DeviceKind::Input);
            debug!("  input device {}: {:?}", i, dev.name());
        }
    }
    #[cfg(feature = "vsock")]
    {
        debug!("number of vsock devices: {}", all_devs.vsock.len());
        for (i, dev) in all_devs.vsock.iter().enumerate() {
            assert_eq!(dev.device_kind(), DeviceKind::Vsock);
            debug!("  vsock device {}: {:?}", i, dev.name());
        }
    }

    all_devs
}
