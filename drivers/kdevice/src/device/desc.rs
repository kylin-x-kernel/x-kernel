// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Common device metadata types shared by the driver framework.

use driver_base::DeviceKind;

use crate::{BusId, DriverId, ResourceSet};

/// Opaque descriptor identifier assigned before a runtime device object exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceDescId(u64);

impl DeviceDescId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw numeric value.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Opaque, globally-unique device identifier assigned by the device manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId(u64);

impl DeviceId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw numeric value.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

impl DeviceState {
    /// Stable short name for the lifecycle state.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discovered => "discovered",
            Self::Matched => "matched",
            Self::Bound => "bound",
            Self::Active => "active",
            Self::Removing => "removing",
            Self::Removed => "removed",
        }
    }
}

/// Where on the bus hierarchy this device lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceLocation {
    /// PCI Bus / Device / Function.
    Pci {
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
    },
    /// MMIO transport (e.g. virtio-mmio).
    Mmio { base: usize, size: usize },
    /// Firmware-described platform device.
    FirmwareNode { id: u16 },
    /// Non-enumerable platform-static device.
    PlatformStatic { id: u16 },
    /// Bus controller / bridge published by a backend (e.g. PCI host bridge).
    ///
    /// Devices at this location are not matched by endpoint drivers; the
    /// owning backend adopts them directly so they can serve as parents for
    /// the endpoints they enumerate.
    Bridge { domain: u16 },
}

/// Where the device description originally came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryOrigin {
    /// Flattened Device Tree.
    DeviceTree,
    /// ACPI tables.
    Acpi,
    /// Hard-coded platform constants.
    PlatformStatic,
}

/// PCI device identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciIdentity {
    pub vendor_id: u16,
    pub device_id: u16,
    pub class: u8,
    pub subclass: u8,
}

/// Platform device identity, carrying firmware and kernel-internal identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformIdentity {
    /// Stable kernel-internal alias used for platform-static fallback devices.
    pub alias: Option<&'static str>,
    /// Raw firmware identity string from DT `compatible` or ACPI `_HID`.
    pub firmware_id: Option<&'static str>,
}

/// Transport-layer information independent of bus/identity.
///
/// Some devices are pure transports of an upper-layer protocol (currently
/// only VirtIO). Carrying the transport descriptor at descriptor level keeps
/// bus identities (`PciIdentity` / `PlatformIdentity`) free of upper-layer
/// concerns and lets matchers branch on transport without re-deriving it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportInfo {
    /// VirtIO transport (PCI or MMIO underneath, distinguished by bus).
    Virtio { device_type: u32 },
}

/// Identity information used for driver matching.
///
/// Each variant corresponds to a bus-type-specific identity structure whose
/// interpretation is owned by the matching domain (`BusTypeObject`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceIdentity {
    /// PCI / PCIe device identity (including VirtIO-over-PCI).
    Pci(PciIdentity),
    /// Platform device identity (firmware-static, DT, ACPI, virtio-mmio).
    Platform(PlatformIdentity),
}

/// Discovery-stage device description.
///
/// A descriptor represents a device candidate and its resources. It is not a
/// runtime device instance and must not carry bound-driver or lifecycle state.
#[derive(Debug, Clone)]
pub struct DeviceDesc {
    id: DeviceDescId,
    bus_id: BusId,
    parent: Option<DeviceId>,
    location: DeviceLocation,
    origin: DiscoveryOrigin,
    identity: DeviceIdentity,
    transport: Option<TransportInfo>,
    resources: ResourceSet,
}

impl DeviceDesc {
    /// Build a new discovery-stage device description.
    pub fn new(
        id: DeviceDescId,
        bus_id: BusId,
        location: DeviceLocation,
        origin: DiscoveryOrigin,
        identity: DeviceIdentity,
        transport: Option<TransportInfo>,
        resources: ResourceSet,
    ) -> Self {
        Self::new_with_parent(
            id, bus_id, None, location, origin, identity, transport, resources,
        )
    }

    /// Build a descriptor that records a parent device for adoption.
    ///
    /// The parent linkage is materialized once the descriptor is published as
    /// a runtime `DeviceObject` (either through the probe path or boot
    /// adoption). Backends use this to publish endpoints as children of a
    /// controller they already adopted.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_parent(
        id: DeviceDescId,
        bus_id: BusId,
        parent: Option<DeviceId>,
        location: DeviceLocation,
        origin: DiscoveryOrigin,
        identity: DeviceIdentity,
        transport: Option<TransportInfo>,
        resources: ResourceSet,
    ) -> Self {
        Self {
            id,
            bus_id,
            parent,
            location,
            origin,
            identity,
            transport,
            resources,
        }
    }

    /// Descriptor ID.
    pub const fn id(&self) -> DeviceDescId {
        self.id
    }

    /// Bus instance this candidate was discovered under.
    pub const fn bus_id(&self) -> BusId {
        self.bus_id
    }

    /// Parent device the descriptor should be attached under, if any.
    pub const fn parent(&self) -> Option<DeviceId> {
        self.parent
    }

    /// Clear the recorded parent linkage.
    ///
    /// Driver-core-internal: used when the parent device is removed so a
    /// later reprobe does not try to attach under a stale parent id.
    pub(crate) fn clear_parent(&mut self) {
        self.parent = None;
    }

    /// Where this candidate lives.
    pub const fn location(&self) -> DeviceLocation {
        self.location
    }

    /// Original description source.
    pub const fn origin(&self) -> DiscoveryOrigin {
        self.origin
    }

    /// Identity used for driver matching.
    pub const fn identity(&self) -> DeviceIdentity {
        self.identity
    }

    /// Transport layer (currently only VirtIO), if any.
    pub const fn transport(&self) -> Option<TransportInfo> {
        self.transport
    }

    /// Resources discovered for this candidate.
    pub fn resources(&self) -> &ResourceSet {
        &self.resources
    }

    /// Clone resources for handoff into a later probe/compatibility path.
    pub fn resources_snapshot(&self) -> ResourceSet {
        self.resources.clone()
    }
}

/// Device lifecycle state tracked by the live device object.
///
/// The `#[repr(u8)]` annotation lets `DeviceObject` store the lifecycle as
/// an `AtomicU8` so hot read paths (`state()`, `is_removing()`) can avoid
/// taking the per-object spinlock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DeviceState {
    /// Discovered on a bus but not yet bound to a driver.
    Discovered = 0,
    /// At least one driver matched and the probe pipeline is evaluating candidates.
    Matched    = 1,
    /// Bound to a driver.
    Bound      = 2,
    /// Activated and available for subsystem consumption.
    Active     = 3,
    /// Removal is in progress (driver.remove and bus-type cleanup running).
    Removing   = 4,
    /// Removed (hot-unplug or driver unbind).
    Removed    = 5,
}

impl DeviceState {
    /// Encoding used by the atomic lifecycle field.
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Decode the atomic lifecycle representation.
    ///
    /// Returns `None` if `raw` is not a valid discriminant. The kernel writes
    /// the atomic only via [`Self::as_u8`], so an invalid value would indicate
    /// memory corruption; callers on lock-free read paths choose a safe
    /// fallback rather than panicking.
    pub const fn from_u8(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::Discovered),
            1 => Some(Self::Matched),
            2 => Some(Self::Bound),
            3 => Some(Self::Active),
            4 => Some(Self::Removing),
            5 => Some(Self::Removed),
            _ => None,
        }
    }
}

/// Metadata-only snapshot of a live device object.
#[derive(Debug, Clone)]
pub struct DeviceRecord {
    pub id: DeviceId,
    pub bus_id: BusId,
    pub parent: Option<DeviceId>,
    /// Bus instance produced by this device, if it is a controller / bridge.
    pub child_bus: Option<BusId>,
    pub location: DeviceLocation,
    pub origin: DiscoveryOrigin,
    pub identity: DeviceIdentity,
    pub transport: Option<TransportInfo>,
    pub resources: ResourceSet,
    pub driver_name: Option<&'static str>,
    pub driver_id: Option<DriverId>,
    pub device_kind: Option<DeviceKind>,
    pub state: DeviceState,
}
