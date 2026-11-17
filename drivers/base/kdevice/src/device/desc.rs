// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Common device metadata types shared by the driver framework.

use core::fmt::Debug;

use driver_base::DeviceKind;
use driver_os_api::DmaInfo;
use fwnode::FwNode;

use crate::{BusId, DriverId, ResourceSet};

/// Opaque descriptor identifier assigned before a runtime device object exists.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DeviceDescId(u64);

impl DeviceDescId {
    /// Wraps a raw numeric descriptor id.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw numeric value.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Opaque, globally-unique device identifier assigned by the device manager.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DeviceId(u64);

impl DeviceId {
    /// Wraps a raw numeric device id.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw numeric value.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Linux-style device number (`dev_t`) encoded from major and minor numbers.
///
/// This is distinct from [`DeviceId`], which identifies a device-model object.
#[derive(Clone, Copy, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DeviceNumber(pub u64);

impl DeviceNumber {
    /// Creates a device number from major and minor components.
    pub const fn new(major: u32, minor: u32) -> Self {
        let major = major as u64;
        let minor = minor as u64;
        Self(
            (major & 0xffff_f000) << 32
                | (major & 0x0000_0fff) << 8
                | (minor & 0xffff_ff00) << 12
                | (minor & 0x0000_00ff),
        )
    }

    /// Returns the major component.
    pub const fn major(self) -> u32 {
        ((self.0 >> 32) & 0xffff_f000 | (self.0 >> 8) & 0x0000_0fff) as u32
    }

    /// Returns the minor component.
    pub const fn minor(self) -> u32 {
        ((self.0 >> 12) & 0xffff_ff00 | self.0 & 0x0000_00ff) as u32
    }
}

impl Debug for DeviceNumber {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DeviceNumber")
            .field("major", &self.major())
            .field("minor", &self.minor())
            .finish()
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceLocation {
    /// PCI Bus / Device / Function.
    Pci {
        /// PCI segment (domain) number.
        segment: u16,
        /// Bus number within the segment.
        bus: u8,
        /// Device number on the bus.
        device: u8,
        /// Function number of the device.
        function: u8,
    },
    /// MMIO transport (e.g. virtio-mmio).
    Mmio {
        /// Physical base address of the transport registers.
        base: usize,
        /// Size of the transport register region in bytes.
        size: usize,
    },
    /// Firmware-described platform device.
    FirmwareNode {
        /// Backend-local firmware node id.
        id: u16,
    },
    /// Non-enumerable platform-static device.
    PlatformStatic {
        /// Stable backend-local id for the static device.
        id: u16,
    },
    /// Bus controller / bridge published by a backend (e.g. PCI host bridge).
    ///
    /// Devices at this location are not matched by endpoint drivers; the
    /// owning backend adopts them directly so they can serve as parents for
    /// the endpoints they enumerate.
    Bridge {
        /// PCI domain (segment) owned by the bridge.
        domain: u16,
    },
}

/// Where the device description originally came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiscoveryOrigin {
    /// Flattened Device Tree.
    DeviceTree,
    /// ACPI tables.
    Acpi,
    /// Hard-coded platform constants.
    PlatformStatic,
}

/// Outcome of resolving a device's legacy INTx route during discovery.
///
/// This is a driver-neutral fact recorded by the discovering bus backend.
/// Firmware-specific errors stay in that backend; consumers use this status
/// only to choose a probe-failure policy. The status describes INTx routing,
/// not whether the device has any usable interrupt (MSI-X may still work).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntxRouteStatus {
    /// A route was resolved and the corresponding [`crate::ResourceDesc::Irq`]
    /// resource was attached to the device.
    Routed,
    /// The device or firmware explicitly reports no legacy interrupt.
    NotConnected,
    /// The route could not be resolved or mapped.
    Failed,
}

/// PCI device identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciIdentity {
    /// PCI vendor id from the configuration-space vendor register.
    pub vendor_id: u16,
    /// PCI device id from the configuration-space device register.
    pub device_id: u16,
    /// PCI base class code.
    pub class: u8,
    /// PCI subclass code within the base class.
    pub subclass: u8,
    /// PCI subsystem vendor id from the configuration-space subsystem register.
    pub subsystem_vendor_id: u16,
    /// PCI subsystem device id from the configuration-space subsystem register.
    pub subsystem_device_id: u16,
}

/// Platform device identity, carrying firmware and kernel-internal identifiers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportInfo {
    /// VirtIO transport (PCI or MMIO underneath, distinguished by bus).
    Virtio {
        /// VirtIO device type code (1 = net, 2 = block, ...).
        device_type: u32,
    },
}

/// Identity information used for driver matching.
///
/// Each variant corresponds to a bus-type-specific identity structure whose
/// interpretation is owned by the matching domain (`BusTypeObject`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceIdentity {
    /// PCI / PCIe device identity (including VirtIO-over-PCI).
    Pci(PciIdentity),
    /// Platform device identity (firmware-static, DT, ACPI, virtio-mmio).
    Platform(PlatformIdentity),
}

/// Input for registering one discovery-stage device.
///
/// A specification is consumed by [`crate::device_desc_add`]. Its fields must
/// be final before registration; the device core assigns the descriptor id and
/// stores its own copy. It contains no runtime lifecycle state and no concrete
/// firmware backend type.
#[derive(Clone, Debug)]
pub struct DeviceDescSpec {
    /// Bus instance the device was discovered on.
    pub bus_id: BusId,
    /// Parent device (controller / bridge), if discovery named one.
    pub parent: Option<DeviceId>,
    /// Where on the bus hierarchy the device lives.
    pub location: DeviceLocation,
    /// Which firmware source described the device.
    pub origin: DiscoveryOrigin,
    /// Bus-specific identity used for driver matching.
    pub identity: DeviceIdentity,
    /// Upper-layer transport descriptor, if any.
    pub transport: Option<TransportInfo>,
    /// Resources (MMIO, IRQ, ...) described for the device.
    pub resources: ResourceSet,
    /// Fully resolved DMA view, when the discovering backend composed one.
    ///
    /// `None` means no view was established; it does not mean unlimited
    /// addressing or coherent DMA.
    pub dma: Option<DmaInfo>,
    /// How the discovering backend resolved this device's legacy INTx route.
    ///
    /// `None` means the backend does not record INTx discovery results; it
    /// does not mean the device has no legacy interrupt.
    pub intx: Option<IntxRouteStatus>,
    /// Firmware node that described this device, if discovery associated one.
    ///
    /// The backend must pass the exact node it already resolved for the
    /// device; registration never searches the firmware tree again. `None`
    /// means no association was established; it does not make the device
    /// invalid. The handle is borrowed, not an owned reference, and only
    /// boot-static backends are supported. Probe consumes it through
    /// [`crate::DeviceObject::firmware_node`].
    pub firmware_node: Option<FwNode<'static>>,
}

/// Discovery-stage device description.
///
/// A descriptor represents one registration and its immutable discovery facts.
/// It is not a runtime device instance and must not carry bound-driver or
/// lifecycle state. It is constructed only by registering a
/// [`DeviceDescSpec`], which also creates the stable [`DeviceId`] documented by
/// [`Self::device_id`].
#[derive(Clone, Debug)]
pub struct DeviceDesc {
    /// Descriptor id assigned by the backend's enumeration context.
    id: DeviceDescId,
    /// Stable runtime object id assigned when the descriptor is registered.
    device_id: DeviceId,
    /// Bus instance the device was discovered on.
    bus_id: BusId,
    /// Parent device (controller / bridge), if discovery named one.
    parent: Option<DeviceId>,
    /// Where on the bus hierarchy the device lives.
    location: DeviceLocation,
    /// Which firmware source described the device.
    origin: DiscoveryOrigin,
    /// Bus-specific identity used for driver matching.
    identity: DeviceIdentity,
    /// Upper-layer transport descriptor, if any.
    transport: Option<TransportInfo>,
    /// Resources (MMIO, IRQ, ...) described for the device.
    resources: ResourceSet,
    /// Fully resolved DMA view, when the discovering backend composed one.
    ///
    /// `None` means no view was established; it does not mean unlimited
    /// addressing or coherent DMA. Bus backends that describe DMA must build
    /// the view from firmware facts and platform policy before registration.
    dma: Option<DmaInfo>,
    /// How the discovering backend resolved this device's legacy INTx route.
    ///
    /// `None` means the backend does not record INTx discovery results; it
    /// does not mean the device has no legacy interrupt.
    intx: Option<IntxRouteStatus>,
    /// Firmware node associated with this device at discovery.
    firmware_node: Option<FwNode<'static>>,
}

impl DeviceDesc {
    /// Build a descriptor from a finalized registration specification.
    ///
    /// Device-core-internal: the only public registration path is
    /// [`crate::device_desc_add`], which assigns the descriptor and stable
    /// device id first.
    pub(crate) fn from_spec(id: DeviceDescId, device_id: DeviceId, spec: DeviceDescSpec) -> Self {
        Self {
            id,
            device_id,
            bus_id: spec.bus_id,
            parent: spec.parent,
            location: spec.location,
            origin: spec.origin,
            identity: spec.identity,
            transport: spec.transport,
            resources: spec.resources,
            dma: spec.dma,
            intx: spec.intx,
            firmware_node: spec.firmware_node,
        }
    }

    /// Legacy INTx route status, if the backend recorded one.
    pub const fn intx_route(&self) -> Option<IntxRouteStatus> {
        self.intx
    }

    /// Resolved DMA view, if the discovering backend composed one.
    pub const fn dma(&self) -> Option<DmaInfo> {
        self.dma
    }

    /// Firmware node associated with this descriptor, if discovery set one.
    ///
    /// Device-core-internal: runtime consumers use
    /// [`crate::DeviceObject::firmware_node`]. The descriptor copy exists so a
    /// retried probe rebuilds the runtime object from the same association.
    pub(crate) fn firmware_node(&self) -> Option<FwNode<'static>> {
        self.firmware_node
    }

    /// Descriptor ID.
    pub const fn id(&self) -> DeviceDescId {
        self.id
    }

    /// Stable runtime object id assigned by registration.
    ///
    /// The descriptor is immutable after registration: every probe retry,
    /// unbind and rebind observes this same id and the same `DeviceObject`
    /// identity. Only unregistering the descriptor ends the registration.
    pub const fn device_id(&self) -> DeviceId {
        self.device_id
    }

    /// Bus instance this candidate was discovered under.
    pub const fn bus_id(&self) -> BusId {
        self.bus_id
    }

    /// Parent device the descriptor should be attached under, if any.
    pub const fn parent(&self) -> Option<DeviceId> {
        self.parent
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

/// Lifecycle state of one binding instance, projected by the stable object.
///
/// A registered object without a current binding projects as `Discovered`; an
/// unregistered object projects as `Removed`. The two forms of `Removed` are
/// distinguishable through [`crate::DeviceObject::is_registered`], but both
/// mean the binding instance is terminal and must never become current again.
///
/// The `#[repr(u8)]` annotation lets `BindingRuntime` store the lifecycle as
/// an `AtomicU8` so hot read paths (`state()`, `is_removing()`) can avoid
/// taking a spinlock.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceState {
    /// Discovered on a bus but not yet bound to a driver.
    Discovered = 0,
    /// A driver matched and its binding metadata was prepared, but the
    /// device has not been announced as bound/active yet.
    ///
    /// Set by the binding-lock holder while the object is still unpublished,
    /// so an observer never sees a device that lacks a driver identity.
    Matched    = 1,
    /// Bound to a driver.
    Bound      = 2,
    /// Activated and available for subsystem consumption.
    Active     = 3,
    /// Removal is in progress (driver.remove and bus-type cleanup running).
    ///
    /// This is not terminal: a removal attempt blocked by outstanding
    /// [`crate::DeviceUse`] admission returns to the previous state.
    Removing   = 4,
    /// Current binding ended, or the whole registration was unregistered.
    ///
    /// For a binding instance this is terminal and irreversible.
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
///
/// The record is assembled from grouped samples rather than one global
/// critical section: `parent`/`child_bus` come from one relations snapshot,
/// while `driver_name`/`driver_id`/`device_kind` come from one binding
/// snapshot. Each group is internally consistent, but the two groups (and the
/// lock-free `state` read) may originate from different instants. Display-only
/// consumers must not treat cross-group combinations as a transaction.
#[derive(Clone, Debug)]
pub struct DeviceRecord {
    /// Globally-unique device id.
    pub id: DeviceId,
    /// Bus instance the device lives on.
    pub bus_id: BusId,
    /// Parent controller / bridge device, from the relations snapshot.
    pub parent: Option<DeviceId>,
    /// Bus instance produced by this device, if it is a controller / bridge.
    /// From the same relations snapshot as `parent`.
    pub child_bus: Option<BusId>,
    /// Where on the bus hierarchy the device lives.
    pub location: DeviceLocation,
    /// Which firmware source described the device.
    pub origin: DiscoveryOrigin,
    /// Bus-specific identity used for driver matching.
    pub identity: DeviceIdentity,
    /// Upper-layer transport descriptor, if any.
    pub transport: Option<TransportInfo>,
    /// Resources (MMIO, IRQ, ...) described for the device.
    pub resources: ResourceSet,
    /// Resolved DMA view, if the discovering backend composed one.
    pub dma: Option<DmaInfo>,
    /// Legacy INTx route status, if the discovering backend recorded one.
    pub intx: Option<IntxRouteStatus>,
    /// Name of the bound driver, once bound. From the binding snapshot.
    pub driver_name: Option<&'static str>,
    /// Id of the bound driver, once bound. From the same binding snapshot.
    pub driver_id: Option<DriverId>,
    /// Device kind reported at publish, once active. From the same binding
    /// snapshot.
    pub device_kind: Option<DeviceKind>,
    /// Lifecycle state sampled before the binding metadata group.
    pub state: DeviceState,
}

unittest::path_mod_test!(tests, "../../ut-tests/device_desc.rs");
