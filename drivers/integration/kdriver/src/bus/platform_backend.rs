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
use driver_os_api::{DmaInfo, IrqOp, IrqResource};
use kdevice::{
    BusId, BusTypeId, DeviceIdentity, DeviceLocation, DiscoveryOrigin, MmioRegion,
    PlatformIdentity, ResourceDesc, ResourceSet,
};
use khal::firmware::devices as fw;
use smallvec::smallvec;

use super::{backend::BusBackend, local_id::LocalIdAlloc};
use crate::{
    driver_registry::virtio::ids::VIRTIO_MMIO_COMPATIBLE, enumeration::EnumerationContext,
};

/// Address capability of a modern direct virtio-mmio transport.
///
/// This is a device-transport constraint, not a firmware window: the queue
/// address registers accept a full-width guest physical address, and the
/// resolved firmware view still bounds every allocation. The scoped QEMU virt
/// policy is the only current producer of such a view; no other device class
/// receives an implicit mask.
const VIRTIO_MMIO_DMA_MASK: u64 = u64::MAX;

/// Unified platform bus backend covering both firmware-described and
/// compile-time-known devices.
pub(crate) struct PlatformBackend {
    id_alloc: LocalIdAlloc,
}

impl PlatformBackend {
    pub(crate) fn new() -> Self {
        Self {
            id_alloc: LocalIdAlloc::new(),
        }
    }

    /// Register a compile-time-known platform device.
    ///
    /// These devices have no firmware description to associate, so the node is
    /// explicitly `None` rather than omitted from the registration path.
    #[cfg(any(
        feature = "serial-ns16550-ioport",
        feature = "ramdisk",
        feature = "ahci",
        feature = "bcm2835-sdhci",
        feature = "sdmmc",
        unittest,
    ))]
    fn register_static_device(
        context: &mut EnumerationContext,
        bus_id: BusId,
        location: DeviceLocation,
        identity: DeviceIdentity,
        resources: ResourceSet,
    ) -> DriverResult<()> {
        context
            .register_device(kdevice::DeviceDescSpec {
                bus_id,
                parent: None,
                location,
                origin: DiscoveryOrigin::PlatformStatic,
                identity,
                transport: None,
                resources,
                dma: None,
                intx: None,
                firmware_node: None,
            })
            .map(|_| ())
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
    /// Register one firmware-described platform device.
    ///
    /// The MMIO declaration is validated before any other side effect: a
    /// malformed declaration rejects the node without allocating an id or
    /// mapping an IRQ. `virtio,mmio` nodes are registered from their
    /// description alone — with an MMIO range they receive a `Mmio` location,
    /// without one a `FirmwareNode` location. No register is accessed here;
    /// the function driver identifies the device during probe. IRQ, DMA and
    /// registration failures return the error so the caller can keep the first
    /// one while continuing to the remaining nodes. The firmware node already
    /// carried by `dev` is stored as-is; this path never searches nodes again.
    fn register_firmware_device(
        &mut self,
        context: &mut EnumerationContext,
        bus_id: BusId,
        dev: &fw::FirmwareDevice,
    ) -> DriverResult<()> {
        let mmio = match dev.mmio {
            Ok(mmio) => mmio,
            Err(error) => {
                log::warn!(
                    "platform: malformed MMIO declaration for {}: {error:?}",
                    dev.firmware_id
                );
                return Err(super::firmware_error_to_driver(error));
            }
        };

        let mut resources: ResourceSet = smallvec![];
        if let Some(mmio) = mmio {
            resources.push(ResourceDesc::Mmio(MmioRegion {
                base: mmio.base,
                size: mmio.size,
            }));
        }

        let identity = DeviceIdentity::Platform(PlatformIdentity {
            alias: None,
            firmware_id: Some(dev.firmware_id),
        });
        let location = match (dev.firmware_id, mmio) {
            (VIRTIO_MMIO_COMPATIBLE, Some(mmio)) => DeviceLocation::Mmio {
                base: mmio.base,
                size: mmio.size,
            },
            _ => DeviceLocation::FirmwareNode {
                id: self.id_alloc.alloc()?,
            },
        };

        match map_firmware_irq(dev) {
            Ok(Some(mapped)) => resources.push(ResourceDesc::Irq(mapped)),
            Ok(None) => {}
            Err(error) => {
                log::warn!(
                    "platform: IRQ mapping failed for {}: {error:?}",
                    dev.firmware_id
                );
                return Err(error);
            }
        }

        let dma = firmware_dma_view(dev);
        let spec = kdevice::DeviceDescSpec {
            bus_id,
            parent: None,
            location,
            origin: discovery_origin(dev.source),
            identity,
            transport: None,
            resources,
            dma,
            intx: None,
            firmware_node: Some(dev.node),
        };
        context
            .register_device(spec)
            .map(|_| ())
            .inspect_err(|err| {
                log::warn!(
                    "platform: failed to register {:?} from {:?}: {:?}",
                    identity,
                    dev.source,
                    err
                );
            })
    }

    fn enumerate_firmware(
        &mut self,
        context: &mut EnumerationContext,
        bus_id: BusId,
    ) -> DriverResult {
        let mut first_error = None;

        let firmware_specs = kdevice::firmware_match_specs_for_bus_type(BusTypeId::PLATFORM);
        #[cfg(feature = "any_firmware_driver")]
        debug_assert!(
            !firmware_specs.is_empty(),
            "platform backend requires firmware-capable platform drivers to be registered before \
             enumeration"
        );

        fw::for_each_compatible(
            |compatible| {
                if compatible == VIRTIO_MMIO_COMPATIBLE {
                    Some(VIRTIO_MMIO_COMPATIBLE)
                } else {
                    firmware_specs
                        .iter()
                        .find_map(|spec| spec.match_dt(compatible))
                }
            },
            |dev| {
                // Production discovery and the helper test share this exact
                // path: the node stored on the descriptor is the one firmware
                // already associated, never a re-lookup by compatible/name.
                if let Err(err) = self.register_firmware_device(context, bus_id, &dev) {
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

/// Compose one firmware device's DMA view.
///
/// Only DT virtio-mmio devices currently have an explicit mask policy. A
/// composition failure is logged and leaves the descriptor without a view, so
/// non-DMA drivers can still bind; it never substitutes an identity window or
/// a coherent default.
fn firmware_dma_view(device: &fw::FirmwareDevice) -> Option<DmaInfo> {
    if device.source != fw::FirmwareSource::DeviceTree
        || device.firmware_id != VIRTIO_MMIO_COMPATIBLE
    {
        return None;
    }
    match crate::firmware_dma::from_firmware_with_qemu_virt_mmio_policy(
        device,
        VIRTIO_MMIO_DMA_MASK,
    ) {
        Ok(dma) => Some(dma),
        Err(error) => {
            log::warn!(
                "platform: no DMA view for {}: {error:?}",
                device.firmware_id
            );
            None
        }
    }
}

/// Map a device's firmware interrupt declaration through the host provider.
///
/// `Ok(None)` means the node declares no interrupt; an unreadable declaration
/// is an error, never an absent interrupt or a raw-number fallback.
fn map_firmware_irq(device: &fw::FirmwareDevice) -> DriverResult<Option<IrqResource>> {
    match device.irq {
        Ok(Some(spec)) => crate::resource::resource_provider()
            .map_fw_irq(spec)
            .map(Some)
            .map_err(crate::resource::map_res_err),
        Ok(None) => Ok(None),
        Err(error) => {
            log::warn!(
                "platform: malformed IRQ declaration for {}: {error:?}",
                device.firmware_id
            );
            Err(super::firmware_error_to_driver(error))
        }
    }
}

fn discovery_origin(source: fw::FirmwareSource) -> DiscoveryOrigin {
    match source {
        fw::FirmwareSource::DeviceTree => DiscoveryOrigin::DeviceTree,
        fw::FirmwareSource::Acpi => DiscoveryOrigin::Acpi,
    }
}

// -- Static enumeration (formerly PlatformStaticBackend) ----------------------

impl PlatformBackend {
    /// `context` and `bus_id` are consumed only inside the per-device `#[cfg]`
    /// blocks below, so a configuration that compiles in no platform-static
    /// device leaves both unused. The names are kept non-underscored because
    /// they are genuine inputs; the allowance below covers configurations
    /// without a matching static device, as in `virtio::activate_virtio_device`.
    #[expect(
        clippy::allow_attributes,
        reason = "CG-024-lint-exceptions: Static enumeration inputs are used only when a matching \
                  platform driver feature is enabled"
    )]
    #[allow(
        unused_variables,
        reason = "CG-024-lint-exceptions: unused_variables: Static enumeration inputs are used \
                  only when a matching platform driver feature is enabled"
    )]
    #[expect(
        clippy::too_many_lines,
        reason = "CG-028-focused-functions: TODO(cg): legacy static platform enumeration; split \
                  per device class"
    )]
    fn enumerate_static(
        &mut self,
        context: &mut EnumerationContext,
        bus_id: BusId,
    ) -> DriverResult {
        // --- NS16550 I/O-port (x86 ISA COM, no device tree) ---
        // Always adopt the early stdout instance; the serial driver matches
        // this alias and reuses the port printk already drives.
        #[cfg(feature = "serial-ns16550-ioport")]
        {
            use kdevice::{DeviceIdentity, DeviceLocation, PlatformIdentity, ResourceSet};
            Self::register_static_device(
                context,
                bus_id,
                DeviceLocation::PlatformStatic {
                    id: self.id_alloc.alloc()?,
                },
                DeviceIdentity::Platform(PlatformIdentity {
                    alias: Some("serial-ns16550-ioport"),
                    firmware_id: None,
                }),
                ResourceSet::new(),
            )?;
        }

        // --- ramdisk ---
        #[cfg(feature = "ramdisk")]
        {
            use kdevice::{DeviceIdentity, DeviceLocation, PlatformIdentity, ResourceSet};
            Self::register_static_device(
                context,
                bus_id,
                DeviceLocation::PlatformStatic {
                    id: self.id_alloc.alloc()?,
                },
                DeviceIdentity::Platform(PlatformIdentity {
                    alias: Some("ramdisk"),
                    firmware_id: None,
                }),
                ResourceSet::new(),
            )?;
        }

        #[cfg(feature = "any_firmware_driver")]
        let firmware_present = khal::firmware::devices::has_device_description();

        // --- AHCI ---
        #[cfg(feature = "ahci")]
        if !firmware_present {
            use kdevice::{
                DeviceIdentity, DeviceLocation, MmioRegion, PlatformIdentity, ResourceDesc,
            };
            use smallvec::smallvec;

            use crate::driver_registry::firmware_specs::AHCI;

            Self::register_static_device(
                context,
                bus_id,
                DeviceLocation::PlatformStatic {
                    id: self.id_alloc.alloc()?,
                },
                DeviceIdentity::Platform(PlatformIdentity {
                    alias: Some(AHCI.alias),
                    firmware_id: None,
                }),
                smallvec![ResourceDesc::Mmio(MmioRegion {
                    base: kbuild_config::AHCI_PADDR,
                    size: 0x1000,
                })],
            )?;
        }

        // --- bcm2835-sdhci ---
        #[cfg(feature = "bcm2835-sdhci")]
        if !firmware_present {
            use kdevice::{DeviceIdentity, DeviceLocation, PlatformIdentity, ResourceSet};

            use crate::driver_registry::firmware_specs::BCM2835_SDHCI;
            Self::register_static_device(
                context,
                bus_id,
                DeviceLocation::PlatformStatic {
                    id: self.id_alloc.alloc()?,
                },
                DeviceIdentity::Platform(PlatformIdentity {
                    alias: Some(BCM2835_SDHCI.alias),
                    firmware_id: None,
                }),
                ResourceSet::new(),
            )?;
        }

        // --- sdmmc ---
        #[cfg(feature = "sdmmc")]
        if !firmware_present {
            use kdevice::{
                DeviceIdentity, DeviceLocation, MmioRegion, PlatformIdentity, ResourceDesc,
            };
            use smallvec::smallvec;

            use crate::driver_registry::firmware_specs::SDMMC;

            Self::register_static_device(
                context,
                bus_id,
                DeviceLocation::PlatformStatic {
                    id: self.id_alloc.alloc()?,
                },
                DeviceIdentity::Platform(PlatformIdentity {
                    alias: Some(SDMMC.alias),
                    firmware_id: None,
                }),
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

unittest::path_mod_test!(
    platform_backend_unittest,
    "../../ut-tests/platform_backend_unittest.rs"
);
unittest::path_mod_test!(
    platform_firmware_node_unittest,
    "../../ut-tests/platform_firmware_node.rs"
);
