// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, format, sync::Arc};

use driver_base::{Device, DeviceKind};
use kdevice::{BusTypeId, DeviceDriver, DeviceMatcher, DeviceObject};

use crate::driver_registry::{BoxedDriver, firmware_specs::SDMMC};

struct SdmmcDriver;

impl DeviceDriver for SdmmcDriver {
    fn name(&self) -> &'static str {
        "sdmmc"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn DeviceMatcher {
        &SDMMC
    }

    fn probe_device(&self, device: Arc<DeviceObject>) -> driver_base::DriverResult<()> {
        let (vaddr, _) = crate::iomap_first_mmio(device.as_ref(), "sdmmc")?;
        let vaddr = vaddr.as_ptr() as usize;

        // SAFETY: `iomap_first_mmio` returned a kernel virtual address that
        // maps the device's first MMIO region described by firmware and
        // installed in `memspace`. The mapping is exclusively owned by this
        // driver instance and lives at least as long as `device`, so the
        // SD/MMC controller registers reachable from `vaddr` satisfy
        // `SdMmcDriver::new`'s precondition of pointing to a valid,
        // exclusively-accessible MMIO window.
        let dev = unsafe { block::sdmmc::SdMmcDriver::new(vaddr) };
        let (index, first_minor) = super::allocate_mmc_disk()?;
        let name = format!("{}{}", dev.name(), index);
        let disk = block::Gendisk::new(name, 179, first_minor, 8, Box::new(dev))?;
        kclass::publish_block(device, Arc::new(disk)).map(drop)
    }
}

pub(super) fn descriptor() -> BoxedDriver {
    Arc::new(SdmmcDriver)
}
