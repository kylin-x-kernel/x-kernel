// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! PCI bus backend for the unified device model.
//!
//! Enumerates PCI devices, **configures their BARs once** so subsequent
//! activate paths can rely on the recorded resources, then registers each
//! device as a long-lived driver-core object.
//!
//! The activate-side bus access (e.g. `virtio::probe_pci_device`) still
//! re-opens a `PciBus`; this is cheap because `pci::iomap_mmio` is idempotent
//! for the ECAM region and `configure_pci_device_if_needed` is a no-op when
//! BARs are already assigned.

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};

use driver_base::DriverResult;
use kdevice::{
    ActiveDeviceAdoption, BusId, BusTypeId, DeviceDriver, DeviceId, DeviceIdentity, DeviceKind,
    DeviceLocation, DeviceMatcher, DeviceObject, DiscoveryOrigin, DriverObject, IoPortRange,
    IrqResource, MmioRegion, NeverMatcher, PciIdentity, PlatformIdentity, ResourceDesc,
    ResourceSet,
};
use lazyinit::LazyInit;
use pci::{Cam, HeaderType, PciBus, PciConfigSource};
use smallvec::SmallVec;

use super::{backend::BusBackend, pci_support::configure_pci_device_if_needed};
use crate::{
    driver_registry::virtio::ids::{VIRTIO_PCI_VENDOR_ID, pci_device_id_to_virtio_type},
    enumeration::EnumerationContext,
};

/// Synthetic driver representing a PCI host bridge instance.
///
/// The host bridge is a *platform* device (its config space is reached
/// through firmware-described ECAM, not through itself), so the driver is
/// declared on [`BusTypeId::PLATFORM`]. It is never matched against
/// descriptors ([`NeverMatcher`]); the PCI backend adopts it directly via
/// [`kdevice::adopt_active_device`] so the host bridge appears in the device
/// tree as the parent of every endpoint the backend enumerates, while the
/// PCI [`kdevice::BusInstance`] records it as its `controller`.
struct PciHostDriver;

impl DeviceDriver for PciHostDriver {
    fn name(&self) -> &'static str {
        "pci-host"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Bus
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn DeviceMatcher {
        &NeverMatcher
    }

    fn probe_device(&self, _device: Arc<DeviceObject>) -> DriverResult<()> {
        Ok(())
    }
}

/// Lazily-registered handle to the shared `PciHostDriver` driver object.
static PCI_HOST_DRIVER: LazyInit<Arc<DriverObject>> = LazyInit::new();

fn pci_host_driver() -> Arc<DriverObject> {
    PCI_HOST_DRIVER.call_once(|| kdevice::register_driver_object(Arc::new(PciHostDriver)));
    PCI_HOST_DRIVER.clone()
}

/// Synthetic driver representing a PCI-to-PCI bridge.
///
/// Like the host bridge, a PCI-to-PCI bridge is adopted directly by the
/// backend (via [`kdevice::adopt_active_device`]) rather than matched
/// through the normal probe pipeline. It uses [`NeverMatcher`] so no
/// endpoint driver will accidentally bind to it.
///
/// The bridge lives on the PCI bus (not the platform bus) because it is a
/// real PCI device with a BDF address.
struct PciBridgeDriver;

impl DeviceDriver for PciBridgeDriver {
    fn name(&self) -> &'static str {
        "pci-bridge"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Bus
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PCI]
    }

    fn matcher(&self) -> &dyn DeviceMatcher {
        &NeverMatcher
    }

    fn probe_device(&self, _device: Arc<DeviceObject>) -> DriverResult<()> {
        Ok(())
    }
}

/// Lazily-registered handle to the shared `PciBridgeDriver` driver object.
static PCI_BRIDGE_DRIVER: LazyInit<Arc<DriverObject>> = LazyInit::new();

fn pci_bridge_driver() -> Arc<DriverObject> {
    PCI_BRIDGE_DRIVER.call_once(|| kdevice::register_driver_object(Arc::new(PciBridgeDriver)));
    PCI_BRIDGE_DRIVER.clone()
}

/// Snapshot of a PCI-to-PCI bridge discovered during enumeration.
struct BridgeEntry {
    bdf: pci::DeviceFunction,
    info: pci::DeviceFunctionInfo,
    secondary_bus: u8,
    /// Upper bound of bus numbers behind this bridge. Not used by the
    /// current flat scan (`0..=bus_end`), but kept for future recursive
    /// enumeration and range validation.
    #[expect(dead_code)]
    subordinate_bus: u8,
}

/// PCI bus discovery backend.
pub struct PciBackend {
    cam: Cam,
}

impl PciBackend {
    /// Create a PCI backend with the given CAM type.
    pub fn new(cam: Cam) -> Self {
        Self { cam }
    }

    /// Auto-detect the right CAM type based on feature flags.
    pub fn auto() -> Self {
        Self::new(super::pci_support::pci_cam_kind())
    }
}

impl BusBackend for PciBackend {
    fn name(&self) -> &'static str {
        "pci"
    }

    fn bus_type_id(&self) -> BusTypeId {
        BusTypeId::PCI
    }

    fn enumerate(&mut self, context: &mut EnumerationContext, bus_id: BusId) -> DriverResult {
        let mut bus = match PciBus::new(self.cam) {
            Ok(bus) => bus,
            Err(err) => {
                log::error!("PCI backend: failed to init bus: {:?}", err);
                return Ok(());
            }
        };

        let source = bus.source();
        let origin = match source {
            PciConfigSource::RuntimeOverride | PciConfigSource::Static => {
                DiscoveryOrigin::PlatformStatic
            }
            PciConfigSource::Firmware => match khal::firmware::devices::pci_host_source() {
                Some(khal::firmware::devices::FirmwareSource::Acpi) => DiscoveryOrigin::Acpi,
                // Default to DeviceTree if firmware reported a host but the
                // source enum didn't disambiguate (only DT/ACPI exist today).
                Some(khal::firmware::devices::FirmwareSource::DeviceTree) | None => {
                    DiscoveryOrigin::DeviceTree
                }
            },
        };

        log::info!(
            "PCI backend: source={:?}, ecam={:#x}, bus_end={:#x}",
            source,
            bus.config_base(),
            bus.bus_end()
        );

        // The PCI host bridge is a platform device that *owns* the PCI bus.
        // Adopt it on the appropriate platform bus and cross-link with the
        // PCI bus so:
        //   * the bridge's device-tree parent is `None` (it sits at the root
        //     of its platform bus),
        //   * the bridge records `child_bus = pci_bus_id`,
        //   * the PCI `BusInstance` records `controller = bridge`,
        //   * every endpoint's `parent` points at the bridge id.
        //
        // If the upstream platform bus is missing for any reason we fall
        // back to a parentless endpoint layout rather than abort enumeration
        // — endpoints still need to be discovered for the system to boot.
        let host_platform_bus_name = "platform";
        let host_parent: Option<DeviceId> = match kdevice::find_bus_by_name(host_platform_bus_name)
        {
            Some(platform_bus) => match kdevice::adopt_active_device(ActiveDeviceAdoption {
                bus_id: platform_bus.id(),
                parent: None,
                location: DeviceLocation::Bridge { domain: 0 },
                origin,
                identity: DeviceIdentity::Platform(PlatformIdentity {
                    alias: Some("pci-host"),
                    firmware_id: None,
                }),
                transport: None,
                resources: SmallVec::new(),
                driver: pci_host_driver(),
            }) {
                Ok(host) => {
                    host.set_child_bus(Some(bus_id));
                    if let Some(pci_bus) = kdevice::find_bus(bus_id) {
                        pci_bus.set_controller(Some(host.clone()));
                    }
                    Some(host.id())
                }
                Err(err) => {
                    log::warn!("PCI backend: host bridge adoption failed: {:?}", err);
                    None
                }
            },
            None => {
                log::warn!(
                    "PCI backend: upstream platform bus {:?} not registered; endpoints will not \
                     have a host-bridge parent",
                    host_platform_bus_name
                );
                None
            }
        };

        let pci_bus_end = bus.bus_end();

        // Pass 1: enumerate every BDF on every bus, collecting standard
        // endpoints and PCI-to-PCI bridges separately. Bridges are adopted
        // as device objects so they appear in the topology as parents for
        // their downstream endpoints.
        let mut endpoints: Vec<(pci::DeviceFunction, pci::DeviceFunctionInfo)> = Vec::new();
        let mut bridges: Vec<BridgeEntry> = Vec::new();
        {
            let (root, config) = bus.parts_mut();
            for bus_nr in 0..=pci_bus_end {
                for (bdf, dev_info) in root.enumerate_bus(bus_nr) {
                    log::debug!("PCI backend: bus {} {bdf}: {dev_info}", bus_nr);
                    match dev_info.header_type {
                        HeaderType::Standard => {
                            endpoints.push((bdf, dev_info));
                        }
                        HeaderType::PciPciBridge => {
                            // Type 1 config header, offset 0x18:
                            //   bits  7:0  Primary Bus Number
                            //   bits 15:8  Secondary Bus Number
                            //   bits 23:16 Subordinate Bus Number
                            let word = config.read_word(bdf, 0x18);
                            let secondary = ((word >> 8) & 0xFF) as u8;
                            let subordinate = ((word >> 16) & 0xFF) as u8;
                            log::info!(
                                "PCI backend: bridge {bdf} secondary={:#x} subordinate={:#x}",
                                secondary,
                                subordinate
                            );
                            bridges.push(BridgeEntry {
                                bdf,
                                info: dev_info,
                                secondary_bus: secondary,
                                subordinate_bus: subordinate,
                            });
                        }
                        _ => {
                            log::debug!(
                                "PCI backend: skipping {bdf} (unsupported header type {:?})",
                                dev_info.header_type
                            );
                        }
                    }
                }
            }
        }

        // Pass 2: adopt PCI-to-PCI bridges as device objects.
        //
        // Each bridge is placed on the PCI bus with its host bridge (or
        // parent bridge) as its parent. A new BusInstance is created for
        // the bridge's secondary bus so that downstream endpoints live on
        // their own bus instance (one BusInstance = one physical bus).
        let mut bridge_parent: BTreeMap<u8, DeviceId> = BTreeMap::new();
        let mut bus_id_map: BTreeMap<u8, BusId> = BTreeMap::new();
        // bus_nr 0 belongs to the root PCI bus.
        bus_id_map.insert(0, bus_id);
        for bridge in &bridges {
            match kdevice::adopt_active_device(ActiveDeviceAdoption {
                bus_id,
                parent: host_parent,
                location: DeviceLocation::Pci {
                    segment: 0,
                    bus: bridge.bdf.bus,
                    device: bridge.bdf.device,
                    function: bridge.bdf.function,
                },
                origin,
                identity: DeviceIdentity::Pci(PciIdentity {
                    vendor_id: bridge.info.vendor_id,
                    device_id: bridge.info.device_id,
                    class: bridge.info.class,
                    subclass: bridge.info.subclass,
                }),
                transport: None,
                resources: SmallVec::new(),
                driver: pci_bridge_driver(),
            }) {
                Ok(bridge_dev) => {
                    log::info!(
                        "PCI backend: adopted bridge {:?} (bus {}->{})",
                        bridge.bdf,
                        bridge.bdf.bus,
                        bridge.secondary_bus
                    );
                    bridge_parent.insert(bridge.secondary_bus, bridge_dev.id());

                    // Create a dedicated BusInstance for the secondary bus
                    // so that downstream devices live on their own bus.
                    if bridge.secondary_bus != 0 {
                        let secondary_bus = kdevice::register_bus_instance(
                            BusTypeId::PCI,
                            // Leak a small string; bus names are static in
                            // the current model.  Using a numeric suffix is
                            // acceptable for a kernel diagnostic name.
                            "pci-bridge-child",
                        );
                        secondary_bus.set_controller(Some(bridge_dev.clone()));
                        bridge_dev.set_child_bus(Some(secondary_bus.id()));
                        bus_id_map.insert(bridge.secondary_bus, secondary_bus.id());
                    }
                }
                Err(err) => {
                    log::warn!(
                        "PCI backend: bridge {:?} adoption failed: {:?}",
                        bridge.bdf,
                        err
                    );
                }
            }
        }

        // Pass 3: configure BARs and gather resources for each endpoint.
        for (bdf, dev_info) in endpoints {
            let mut resources: ResourceSet = SmallVec::new();

            // Configure BARs up front so address==0 unassigned BARs become
            // real addresses before we record them; probe paths can then use
            // the device resources without re-opening the bus for BAR setup.
            //
            // If configuration fails, the device may be left with some BARs
            // assigned and others at zero, and the command register in an
            // inconsistent state. Registering such a device would leak a
            // half-broken object into the driver model — subscribers may try
            // to bind it and dereference invalid MMIO. Skip the device
            // entirely; a subsequent reprobe (after firmware fix or window
            // expansion) can pick it up.
            {
                let (root, _) = bus.parts_mut();
                if let Err(err) = configure_pci_device_if_needed(root, bdf) {
                    log::warn!(
                        "PCI backend: configure {:?} failed: {:?}; skipping device",
                        bdf,
                        err
                    );
                    continue;
                }
            }

            let (root, config) = bus.parts_mut();
            let mut bar_idx = 0u8;
            while bar_idx < 6 {
                let info = match root.bar_info(bdf, bar_idx) {
                    Ok(Some(info)) => info,
                    _ => {
                        bar_idx += 1;
                        continue;
                    }
                };
                // 64-bit Memory BARs occupy two consecutive slots; the upper
                // slot is just the high 32 bits of the address, not a separate
                // BAR. Advance past it so we don't mis-report it as a stray
                // "unassigned 32-bit BAR".
                let advance = if info.takes_two_entries() { 2 } else { 1 };

                match info {
                    pci::BarInfo::Memory { address, size, .. } if size > 0 => {
                        if address == 0 {
                            // Still unassigned (no allocator window). Skip
                            // rather than recording PA:0 which would later
                            // mis-map page zero.
                            log::warn!(
                                "PCI backend: {:?} BAR{} unassigned after configure",
                                bdf,
                                bar_idx
                            );
                        } else {
                            resources.push(ResourceDesc::Mmio(MmioRegion {
                                base: address as usize,
                                size: size as usize,
                            }));
                        }
                    }
                    pci::BarInfo::IO { address, size } if size > 0 && address > 0 => {
                        // PCI IO BARs are 32-bit; `pci::BarInfo::IO::address`
                        // is already in PIO space.
                        let base = (address & 0xffff) as u16;
                        let nports = (size & 0xffff) as u16;
                        resources.push(ResourceDesc::IoPort(IoPortRange { base, size: nports }));
                    }
                    _ => {}
                }

                bar_idx += advance;
            }

            // Legacy INTx routing — only meaningful for devices that have not
            // negotiated MSI/MSI-X. Best-effort: errors from firmware lookup
            // are silently ignored because most modern devices use MSI-X.
            if let Some(route) = pci::legacy_interrupt_route(config, bdf) {
                resources.push(ResourceDesc::Irq(IrqResource::new(
                    route.irq,
                    kdevice::irq_trigger_from_firmware(route.trigger),
                )));
            }

            let location = DeviceLocation::Pci {
                segment: 0,
                bus: bdf.bus,
                device: bdf.device,
                function: bdf.function,
            };

            // Assign the correct parent and bus: if this endpoint lives
            // on a secondary bus behind a PCI-to-PCI bridge, the bridge
            // device is its parent and the secondary BusInstance is its
            // bus; otherwise the host bridge and root PCI bus apply.
            let parent = bridge_parent.get(&bdf.bus).copied().or(host_parent);
            let ep_bus = bus_id_map.get(&bdf.bus).copied().unwrap_or(bus_id);

            // Virtio-over-PCI uses the PCI identity for matching but carries
            // the upper-layer transport descriptor at descriptor level so the
            // PCI identity stays free of upper-layer concerns.
            let transport = match (
                dev_info.vendor_id,
                pci_device_id_to_virtio_type(dev_info.device_id),
            ) {
                (VIRTIO_PCI_VENDOR_ID, Some(device_type)) => {
                    Some(kdevice::TransportInfo::Virtio { device_type })
                }
                _ => None,
            };
            let identity = DeviceIdentity::Pci(PciIdentity {
                vendor_id: dev_info.vendor_id,
                device_id: dev_info.device_id,
                class: dev_info.class,
                subclass: dev_info.subclass,
            });

            context.register_device_with_parent(
                ep_bus, parent, location, origin, identity, transport, resources,
            )?;
        }

        Ok(())
    }
}
