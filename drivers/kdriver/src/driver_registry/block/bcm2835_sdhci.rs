// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc};

use driver_base::{DeviceKind, DriverError};
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
            Ok(dev) => kclass::publish_block(device, Box::new(dev)),
            Err(_) => Err(DriverError::Io),
        }
    }
}

pub(super) fn descriptor() -> BoxedDriver {
    Arc::new(BcmSdhciDriver)
}
