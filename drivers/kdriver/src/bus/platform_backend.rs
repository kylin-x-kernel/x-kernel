// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unified platform bus backend.
//!
//! Combines firmware-described devices (walked via
//! [`khal::firmware::devices::for_each_compatible`]) and well-known
//! platform-static devices (ramdisk, AHCI, etc.) into a single `platform` bus.
//!
//! All firmware-flavor knowledge (DT vs ACPI) lives behind the
//! `khal::firmware::devices` abstraction, so this backend stays
//! firmware-agnostic.

use driver_base::DriverResult;
use kdevice::{
    BusId, BusTypeId, DeviceIdentity, DeviceLocation, DiscoveryOrigin, IrqResource, MmioRegion,
    PlatformIdentity, ResourceDesc, ResourceSet,
};
use khal::firmware::devices as fw;
use smallvec::smallvec;

use super::{backend::BusBackend, local_id::LocalIdAlloc};
use crate::enumeration::EnumerationContext;

const VIRTIO_MMIO_DT_COMPATIBLE: &str = "virtio,mmio";

/// Unified platform bus backend covering both firmware-described and
/// compile-time-known devices.
pub struct PlatformBackend {
    id_alloc: LocalIdAlloc,
    #[cfg(feature = "console")]
    boot_console_adopted: bool,
}

impl PlatformBackend {
    pub fn new() -> Self {
        Self {
            id_alloc: LocalIdAlloc::new(),
            #[cfg(feature = "console")]
            boot_console_adopted: false,
        }
    }
}

impl BusBackend for PlatformBackend {
    fn name(&self) -> &'static str {
        "platform"
    }

    fn bus_type_id(&self) -> BusTypeId {
        BusTypeId::PLATFORM
    }

    fn enumerate(&mut self, context: &mut EnumerationContext, bus_id: BusId) -> DriverResult {
        self.enumerate_firmware(context, bus_id)?;
        self.enumerate_static(context, bus_id)?;
        Ok(())
    }
}

// -- Firmware enumeration (formerly PlatformFirmwareBackend) ------------------

impl PlatformBackend {
    fn enumerate_firmware(
        &mut self,
        context: &mut EnumerationContext,
        bus_id: BusId,
    ) -> DriverResult {
        let mut first_error = None;

        #[cfg(feature = "console")]
        if !self.boot_console_adopted && console_driver::config().is_some() {
            let location_id = self.id_alloc.alloc();
            self.boot_console_adopted =
                crate::driver_registry::char::adopt_boot_console(bus_id, location_id)?;
        }

        let firmware_specs = kdevice::firmware_match_specs_for_bus_type(BusTypeId::PLATFORM);
        #[cfg(feature = "any_firmware_driver")]
        debug_assert!(
            !firmware_specs.is_empty(),
            "platform backend requires firmware-capable platform drivers to be registered before \
             enumeration"
        );

        fw::for_each_compatible(
            |compatible| {
                if compatible == VIRTIO_MMIO_DT_COMPATIBLE {
                    Some(VIRTIO_MMIO_DT_COMPATIBLE)
                } else {
                    firmware_specs
                        .iter()
                        .find_map(|spec| spec.match_dt(compatible))
                }
            },
            |dev| {
                let mut resources: ResourceSet = smallvec![];
                if let Some(mmio) = dev.mmio {
                    resources.push(ResourceDesc::Mmio(MmioRegion {
                        base: mmio.base,
                        size: mmio.size,
                    }));
                }
                if let Some(irq) = dev.irq {
                    resources.push(ResourceDesc::Irq(IrqResource::new(
                        irq.irq,
                        kdevice::irq_trigger_from_firmware(irq.trigger),
                    )));
                }

                let registration = if dev.firmware_id == VIRTIO_MMIO_DT_COMPATIBLE {
                    match dev.mmio {
                        #[cfg(feature = "virtio")]
                        Some(mmio) => virtio_mmio_registration(mmio),
                        #[cfg(not(feature = "virtio"))]
                        Some(_) => None,
                        None => {
                            log::warn!(
                                "platform: skipping {} node without MMIO resource",
                                VIRTIO_MMIO_DT_COMPATIBLE
                            );
                            None
                        }
                    }
                } else {
                    Some((
                        DeviceLocation::FirmwareNode {
                            id: self.id_alloc.alloc(),
                        },
                        DeviceIdentity::Platform(PlatformIdentity {
                            alias: None,
                            firmware_id: Some(dev.firmware_id),
                        }),
                        None,
                    ))
                };

                let Some((location, identity, transport)) = registration else {
                    return;
                };

                if let Err(err) = context.register_device(
                    bus_id,
                    location,
                    discovery_origin(dev.source),
                    identity,
                    transport,
                    resources,
                ) {
                    log::warn!(
                        "platform: failed to register {:?} from {:?}: {:?}",
                        identity,
                        dev.source,
                        err
                    );
                    first_error.get_or_insert(err);
                }
            },
        );

        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

#[cfg(feature = "virtio")]
fn virtio_mmio_registration(
    mmio: fw::MmioResource,
) -> Option<(
    DeviceLocation,
    DeviceIdentity,
    Option<kdevice::TransportInfo>,
)> {
    let regs = match crate::iomap_mmio(mmio.base, mmio.size, "virtio-mmio-discovery") {
        Ok(regs) => regs,
        Err(err) => {
            log::warn!(
                "platform: failed to map {} at {:#x}: {:?}",
                VIRTIO_MMIO_DT_COMPATIBLE,
                mmio.base,
                err
            );
            return None;
        }
    };

    // SAFETY: `regs` was obtained from `iomap_mmio` which maps a valid
    // physical MMIO region, and `mmio.size` matches the region size.
    let Some((device_kind, _transport)) =
        (unsafe { virtio::probe_mmio_device(regs.as_ptr(), mmio.size) })
    else {
        log::trace!(
            "platform: skipping empty {} slot at {:#x}",
            VIRTIO_MMIO_DT_COMPATIBLE,
            mmio.base,
        );
        return None;
    };

    let Some(device_type) =
        crate::driver_registry::virtio::ids::device_kind_to_virtio_type(device_kind)
    else {
        log::debug!(
            "platform: skipping unsupported {} kind {:?} at {:#x}",
            VIRTIO_MMIO_DT_COMPATIBLE,
            device_kind,
            mmio.base,
        );
        return None;
    };

    Some((
        DeviceLocation::Mmio {
            base: mmio.base,
            size: mmio.size,
        },
        DeviceIdentity::Platform(PlatformIdentity {
            alias: None,
            firmware_id: Some(VIRTIO_MMIO_DT_COMPATIBLE),
        }),
        Some(kdevice::TransportInfo::Virtio { device_type }),
    ))
}

fn discovery_origin(source: fw::FirmwareSource) -> DiscoveryOrigin {
    match source {
        fw::FirmwareSource::DeviceTree => DiscoveryOrigin::DeviceTree,
        fw::FirmwareSource::Acpi => DiscoveryOrigin::Acpi,
    }
}

// -- Static enumeration (formerly PlatformStaticBackend) ----------------------

impl PlatformBackend {
    fn enumerate_static(
        &mut self,
        _context: &mut EnumerationContext,
        _bus_id: BusId,
    ) -> DriverResult {
        // --- ramdisk ---
        #[cfg(feature = "ramdisk")]
        {
            use kdevice::{
                DeviceIdentity, DeviceLocation, DiscoveryOrigin, PlatformIdentity, ResourceSet,
            };
            _context.register_device(
                _bus_id,
                DeviceLocation::PlatformStatic {
                    id: self.id_alloc.alloc(),
                },
                DiscoveryOrigin::PlatformStatic,
                DeviceIdentity::Platform(PlatformIdentity {
                    alias: Some("ramdisk"),
                    firmware_id: None,
                }),
                None,
                ResourceSet::new(),
            )?;
        }

        #[cfg(feature = "any_firmware_driver")]
        let firmware_present = khal::firmware::devices::has_device_description();

        // --- AHCI ---
        #[cfg(feature = "ahci")]
        if !firmware_present {
            use kdevice::{
                DeviceIdentity, DeviceLocation, DiscoveryOrigin, MmioRegion, PlatformIdentity,
                ResourceDesc,
            };
            use smallvec::smallvec;

            use crate::driver_registry::firmware_specs::AHCI;

            _context.register_device(
                _bus_id,
                DeviceLocation::PlatformStatic {
                    id: self.id_alloc.alloc(),
                },
                DiscoveryOrigin::PlatformStatic,
                DeviceIdentity::Platform(PlatformIdentity {
                    alias: Some(AHCI.alias),
                    firmware_id: None,
                }),
                None,
                smallvec![ResourceDesc::Mmio(MmioRegion {
                    base: kbuild_config::AHCI_PADDR,
                    size: 0x1000,
                })],
            )?;
        }

        // --- bcm2835-sdhci ---
        #[cfg(feature = "bcm2835-sdhci")]
        if !firmware_present {
            use kdevice::{
                DeviceIdentity, DeviceLocation, DiscoveryOrigin, PlatformIdentity, ResourceSet,
            };

            use crate::driver_registry::firmware_specs::BCM2835_SDHCI;
            _context.register_device(
                _bus_id,
                DeviceLocation::PlatformStatic {
                    id: self.id_alloc.alloc(),
                },
                DiscoveryOrigin::PlatformStatic,
                DeviceIdentity::Platform(PlatformIdentity {
                    alias: Some(BCM2835_SDHCI.alias),
                    firmware_id: None,
                }),
                None,
                ResourceSet::new(),
            )?;
        }

        // --- sdmmc ---
        #[cfg(feature = "sdmmc")]
        if !firmware_present {
            use kdevice::{
                DeviceIdentity, DeviceLocation, DiscoveryOrigin, MmioRegion, PlatformIdentity,
                ResourceDesc,
            };
            use smallvec::smallvec;

            use crate::driver_registry::firmware_specs::SDMMC;

            _context.register_device(
                _bus_id,
                DeviceLocation::PlatformStatic {
                    id: self.id_alloc.alloc(),
                },
                DiscoveryOrigin::PlatformStatic,
                DeviceIdentity::Platform(PlatformIdentity {
                    alias: Some(SDMMC.alias),
                    firmware_id: None,
                }),
                None,
                smallvec![ResourceDesc::Mmio(MmioRegion {
                    base: kbuild_config::SDMMC_PADDR,
                    size: 0x1000,
                })],
            )?;
        }

        // --- fxmac ---
        // The fxmac driver requires a firmware-provided MMIO window; there
        // is no platform-static base for it in `kbuild_config`. If firmware
        // didn't describe it, skip registration entirely and report loudly
        // rather than registering an empty resource set that would later
        // bottom out at address 0 in the activate path.
        #[cfg(feature = "fxmac")]
        if !firmware_present {
            log::warn!(
                "fxmac: no firmware description and no platform-static MMIO base; skipping \
                 platform fallback registration"
            );
        }

        Ok(())
    }
}
