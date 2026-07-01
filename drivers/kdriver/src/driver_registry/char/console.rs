// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc};

use char_driver::CharDevice;
use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use kdevice::{
    ActiveDeviceAdoption, BusId, BusTypeId, CompatibleAliasMatcher, DeviceDriver, DeviceIdentity,
    DeviceLocation, DeviceMatcher, DeviceObject, DiscoveryOrigin, IoPortRange, IrqResource,
    MmioRegion, PlatformIdentity, ResourceDesc,
};
use smallvec::smallvec;

use crate::driver_registry::BoxedDriver;

struct RuntimeConsole;

impl Device for RuntimeConsole {
    fn name(&self) -> &str {
        "console"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }

    fn irq(&self) -> Option<usize> {
        console_driver::interrupt_id()
    }
}

impl CharDevice for RuntimeConsole {
    fn read(&self, buf: &mut [u8]) -> DriverResult<usize> {
        Ok(console_driver::read_data(buf))
    }

    fn write(&self, buf: &[u8]) -> DriverResult<usize> {
        console_driver::write_data(buf);
        Ok(buf.len())
    }
}

pub(crate) fn adopt_boot_console(bus_id: BusId, location_id: u16) -> DriverResult<bool> {
    let Some(config) = console_driver::config() else {
        return Ok(false);
    };
    let driver = kdevice::find_driver_by_name("console").ok_or(DriverError::BadState)?;

    let (location, origin) = match config.source {
        console_driver::ConsoleSource::DeviceTree => (
            DeviceLocation::FirmwareNode { id: location_id },
            DiscoveryOrigin::DeviceTree,
        ),
        console_driver::ConsoleSource::Acpi => (
            DeviceLocation::FirmwareNode { id: location_id },
            DiscoveryOrigin::Acpi,
        ),
        console_driver::ConsoleSource::PlatformStatic => (
            DeviceLocation::PlatformStatic { id: location_id },
            DiscoveryOrigin::PlatformStatic,
        ),
    };

    let mut resources = match config.transport {
        console_driver::ConsoleTransport::Mmio { paddr, size } => {
            smallvec![ResourceDesc::Mmio(MmioRegion {
                base: paddr.as_usize(),
                size,
            })]
        }
        console_driver::ConsoleTransport::IoPort { io_port } => {
            smallvec![ResourceDesc::IoPort(IoPortRange {
                base: io_port,
                size: 8,
            })]
        }
    };
    if let Some(desc) = config.irq {
        resources.push(ResourceDesc::Irq(IrqResource::new(
            desc.hwirq,
            kdevice::irq_trigger_from_khal(desc.trigger),
        )));
    }

    let device = kdevice::adopt_active_device(ActiveDeviceAdoption {
        bus_id,
        parent: None,
        location,
        origin,
        identity: DeviceIdentity::Platform(PlatformIdentity {
            alias: Some("console"),
            firmware_id: None,
        }),
        transport: None,
        resources,
        driver,
    })?;
    console_driver::register_input_irq_handler();
    console_driver::runtime::register_console_runtime(device, Box::new(RuntimeConsole))?;
    Ok(true)
}

struct ConsoleDriver;

impl DeviceDriver for ConsoleDriver {
    fn name(&self) -> &'static str {
        "console"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn DeviceMatcher {
        static M: CompatibleAliasMatcher = CompatibleAliasMatcher("console");
        &M
    }

    fn probe_device(&self, device: Arc<DeviceObject>) -> DriverResult<()> {
        if console_driver::config().is_none() {
            return Err(DriverError::BadState);
        }
        console_driver::register_input_irq_handler();
        console_driver::runtime::register_console_runtime(device, Box::new(RuntimeConsole))
    }
}

pub(super) fn descriptor() -> BoxedDriver {
    Arc::new(ConsoleDriver)
}
