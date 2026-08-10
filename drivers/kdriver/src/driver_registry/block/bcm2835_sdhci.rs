// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, format, sync::Arc};

use driver_base::{Device, DeviceKind, DriverError};
use kdevice::{BusTypeId, DeviceDriver, DeviceMatcher, DeviceObject};

use crate::driver_registry::{BoxedDriver, firmware_specs::BCM2835_SDHCI};

struct BcmSdhciDriver;

impl DeviceDriver for BcmSdhciDriver {
    fn name(&self) -> &'static str {
        "bcm2835-sdhci"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn DeviceMatcher {
        &BCM2835_SDHCI
    }

    fn probe_device(&self, device: Arc<DeviceObject>) -> driver_base::DriverResult<()> {
        match block::bcm2835sdhci::SDHCIDriver::try_new() {
            Ok(dev) => {
                let (index, first_minor) = super::allocate_mmc_disk()?;
                let name = format!("{}{}", dev.name(), index);
                let disk = block::Gendisk::new(name, 179, first_minor, 8, Box::new(dev))?;
                kclass::publish_block(device, Arc::new(disk)).map(drop)
            }
            Err(_) => Err(DriverError::Io),
        }
    }
}

pub(super) fn descriptor() -> BoxedDriver {
    Arc::new(BcmSdhciDriver)
}
