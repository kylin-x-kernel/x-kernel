// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc};

use driver_base::{DeviceKind, DriverError};
use kdevice::{BusTypeId, DeviceDriver, DeviceMatcher, DeviceObject, PciDeviceId, PciIdsMatcher};

use crate::driver_registry::BoxedDriver;

static IXGBE_MATCH: PciIdsMatcher = PciIdsMatcher(&[PciDeviceId {
    vendor_id: net::ixgbe::INTEL_VEND,
    device_id: net::ixgbe::INTEL_82599,
}]);

struct IxgbeDriver;

impl DeviceDriver for IxgbeDriver {
    fn name(&self) -> &'static str {
        "ixgbe"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Net
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PCI]
    }

    fn matcher(&self) -> &dyn DeviceMatcher {
        &IXGBE_MATCH
    }

    fn probe_device(&self, device: Arc<DeviceObject>) -> driver_base::DriverResult<()> {
        let (vaddr, mmio_size) = crate::iomap_first_mmio(device.as_ref(), "ixgbe-bar")?;
        let vaddr = vaddr.as_ptr() as usize;

        match net::ixgbe::IxgbeNic::<super::ixgbe_hal::IxgbeHalImpl, 1024, 1>::init(
            vaddr, mmio_size,
        ) {
            Ok(nic) => kclass::publish_net(device, Box::new(nic)),
            Err(_) => Err(DriverError::Io),
        }
    }
}

pub(super) fn descriptor() -> BoxedDriver {
    Arc::new(IxgbeDriver)
}
