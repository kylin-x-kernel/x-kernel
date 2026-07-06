// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc};

use driver_base::DeviceKind;
use kdevice::{BusTypeId, CompatibleAliasMatcher, DeviceDriver, DeviceMatcher, DeviceObject};

use crate::driver_registry::BoxedDriver;

static RAMDISK_MATCH: CompatibleAliasMatcher = CompatibleAliasMatcher("ramdisk");

struct RamdiskDriver;

impl DeviceDriver for RamdiskDriver {
    fn name(&self) -> &'static str {
        "ramdisk"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn DeviceMatcher {
        &RAMDISK_MATCH
    }

    fn probe_device(&self, device: Arc<DeviceObject>) -> driver_base::DriverResult<()> {
        // `ramdisk-static` backs the device with a build-time filesystem image
        // (embedded, zero-copy) so it can be mounted as a real root filesystem.
        // Otherwise the device is an empty zero-filled region, useful only for
        // driver bring-up. The two arms produce different concrete types, so
        // this must be `#[cfg]` attributes rather than `if cfg!()`.
        #[cfg(feature = "ramdisk-static")]
        let dev = block::ramdisk_image::ramdisk();
        #[cfg(not(feature = "ramdisk-static"))]
        let dev = block::ramdisk::RamDisk::new(0x100_0000); // 16 MiB

        kclass::publish_block(device, Box::new(dev))
    }
}

pub(super) fn descriptor() -> BoxedDriver {
    Arc::new(RamdiskDriver)
}
