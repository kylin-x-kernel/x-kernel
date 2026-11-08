// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! PCI legacy INTx route resolution for x86.
//!
//! This is the PCI integration seam: it reads the Interrupt Pin register, asks
//! [`acpi_owner`] for the published firmware route, and maps that route
//! through the resource provider. It returns either the mapped resource or a
//! driver-neutral discovery status. Firmware-specific failures are logged here
//! and never escape as ACPI types.

use driver_os_api::{IrqOp, IrqResource};
use kdevice::{DiscoveryOrigin, IntxRouteStatus};
use pci::{DeviceFunction, PciConfigAccess};

use super::{acpi_owner, map_pci_legacy_irq};

/// Resolves one function's legacy INTx route.
///
/// `Ok(resource)` is a mapped IRQ, `Err(NotConnected)` means the device or
/// firmware explicitly reports no legacy interrupt, and `Err(Failed)` means
/// the route could not be read, queried, or mapped. The failure detail is
/// logged here; the caller stores the neutral status with the device.
pub(super) fn resolve(
    config: &PciConfigAccess,
    bdf: DeviceFunction,
    origin: DiscoveryOrigin,
) -> Result<IrqResource, IntxRouteStatus> {
    let pin = match pci::interrupt_pin(config, bdf) {
        Ok(pin) => pin,
        Err(error) => {
            log::warn!("PCI {bdf:?}: Interrupt Pin read failed: {error:?}");
            return Err(IntxRouteStatus::Failed);
        }
    };
    if pin == 0 || pin == 0xff {
        return Err(IntxRouteStatus::NotConnected);
    }
    if origin != DiscoveryOrigin::Acpi {
        log::warn!("PCI {bdf:?}: INTx pin {pin} present but routing origin is {origin:?}");
        return Err(IntxRouteStatus::Failed);
    }

    let route = match acpi_owner::route(bdf, pin) {
        Ok(Some(route)) => route,
        Ok(None) => return Err(IntxRouteStatus::NotConnected),
        Err(error) => {
            log::warn!("PCI {bdf:?}: INTx route lookup failed: {error:?}");
            return Err(IntxRouteStatus::Failed);
        }
    };

    let provider = crate::resource::resource_provider();
    match map_pci_legacy_irq(
        route,
        |route| provider.map_irq(route),
        |gsi, trigger| provider.map_gsi(gsi, trigger),
    ) {
        Ok(irq) => Ok(irq),
        Err(error) => {
            log::warn!("PCI {bdf:?}: INTx mapping failed: {error:?}");
            Err(IntxRouteStatus::Failed)
        }
    }
}

unittest::path_mod_test!(pci_intx_unittest, "../../ut-tests/pci_intx_unittest.rs");
