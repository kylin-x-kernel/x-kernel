// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Long-lived device object, equivalent to the kernel's central device fact.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::ops::Deref;

use driver_base::DeviceKind;
use driver_os_api::DmaInfo;
use fwnode::FwNode;
use kspin::SpinNoPreempt;

use super::binding::{BindingRef, BindingRuntime};
use crate::{
    BusId, DeviceId, DeviceIdentity, DeviceLocation, DeviceRecord, DeviceState, DiscoveryOrigin,
    DriverId, IntxRouteStatus, ResourceDesc, ResourceSet, TransportInfo,
};

/// Topology relations of a device object protected by a per-object lock.
///
/// These are device facts managed by the lifecycle layer, not by the binding
/// runtime: `BindingRuntime` does not own or clear them. The existing shared
/// removal path still detaches the device from its parent and clears the
/// parent pointer of its surviving children.
struct DeviceRelations {
    parent: Option<DeviceId>,
    children: Vec<DeviceId>,
    /// Bus instance this device produces, if it is a bus controller / bridge.
    ///
    /// For a PCI host bridge this points at the PCI bus instance whose
    /// endpoints sit beneath the bridge in the device tree. Endpoint and
    /// leaf devices leave this `None`.
    child_bus: Option<BusId>,
}

/// Long-lived device object owned by the driver core.
pub struct DeviceObject {
    id: DeviceId,
    bus_id: BusId,
    location: DeviceLocation,
    origin: DiscoveryOrigin,
    identity: DeviceIdentity,
    transport: Option<TransportInfo>,
    resources: ResourceSet,
    /// Resolved DMA view carried over from discovery, if any.
    dma: Option<DmaInfo>,
    /// Legacy INTx route status carried over from discovery, if any.
    intx: Option<IntxRouteStatus>,
    /// Firmware node associated with this device at discovery, if any.
    firmware_node: Option<FwNode<'static>>,
    /// Device-tree relations; owned by the lifecycle layer, not by the binding
    /// runtime.
    relations: SpinNoPreempt<DeviceRelations>,
    /// Runtime state, in-use admission, driver metadata and cleanup batch of
    /// this object's single driver binding.
    ///
    /// The object has no replace/reset path for this binding. It is held in
    /// `Arc` only so read-only [`BindingRef`] handles and [`DeviceUse`] guards
    /// can retain the exact instance they were created from; lifecycle,
    /// usage and devres still have this single owner. See [`DeviceUse`] for
    /// the admission contract.
    binding: Arc<BindingRuntime>,
}

impl DeviceObject {
    /// Creates an unpublished, bound but inactive device for class-layer tests.
    ///
    /// No registry entry, event or hardware resource is created. The caller
    /// owns the fixture's lifetime and must not publish its synthetic identity.
    #[cfg(unittest)]
    pub fn new_bound_for_test(id: DeviceId, kind: DeviceKind) -> Self {
        let device = Self::new(
            id,
            BusId::new(0),
            DeviceLocation::PlatformStatic { id: 0 },
            DiscoveryOrigin::PlatformStatic,
            DeviceIdentity::Platform(crate::PlatformIdentity {
                alias: Some("test-device"),
                firmware_id: None,
            }),
            None,
            ResourceSet::new(),
        );
        device.bind_driver(DriverId::new(0), "test-driver", kind);
        device
    }

    /// Create a newly discovered device object.
    pub fn new(
        id: DeviceId,
        bus_id: BusId,
        location: DeviceLocation,
        origin: DiscoveryOrigin,
        identity: DeviceIdentity,
        transport: Option<TransportInfo>,
        resources: ResourceSet,
    ) -> Self {
        Self {
            id,
            bus_id,
            location,
            origin,
            identity,
            transport,
            resources,
            dma: None,
            intx: None,
            firmware_node: None,
            relations: SpinNoPreempt::new(DeviceRelations {
                parent: None,
                children: Vec::new(),
                child_bus: None,
            }),
            binding: Arc::new(BindingRuntime::new()),
        }
    }

    /// Unique device ID.
    pub const fn id(&self) -> DeviceId {
        self.id
    }

    /// Owning bus instance ID.
    pub const fn bus_id(&self) -> BusId {
        self.bus_id
    }

    /// Bus-level location.
    pub const fn location(&self) -> DeviceLocation {
        self.location
    }

    /// Firmware source that described the device.
    pub const fn origin(&self) -> DiscoveryOrigin {
        self.origin
    }

    /// Matching identity.
    pub const fn identity(&self) -> DeviceIdentity {
        self.identity
    }

    /// Transport layer (currently only VirtIO), if any.
    pub const fn transport(&self) -> Option<TransportInfo> {
        self.transport
    }

    /// Attach the DMA view resolved during discovery.
    ///
    /// This is a construction-time builder; publish the object only after the
    /// view is final. `None` means discovery established no view, not that the
    /// device is unlimited or coherent.
    pub fn with_dma(mut self, dma: Option<DmaInfo>) -> Self {
        self.dma = dma;
        self
    }

    /// Attach the legacy INTx discovery status.
    ///
    /// This is a construction-time builder; publish the object only after the
    /// status is final. `None` means no backend record exists, not that the
    /// device has a legacy interrupt.
    pub fn with_intx_route(mut self, status: Option<IntxRouteStatus>) -> Self {
        self.intx = status;
        self
    }

    /// Attach the firmware node recorded for this device at discovery.
    ///
    /// This is a construction-time builder used by the device core when it
    /// rebuilds a runtime object from its descriptor. There is no
    /// post-publication replacement path. The handle is borrowed, not owned.
    pub(crate) fn with_firmware_node(mut self, node: Option<FwNode<'static>>) -> Self {
        self.firmware_node = node;
        self
    }

    /// Borrow hardware resources associated with this device.
    pub fn resources(&self) -> &ResourceSet {
        &self.resources
    }

    /// Resolved DMA view, if discovery established one.
    pub const fn dma(&self) -> Option<DmaInfo> {
        self.dma
    }

    /// Legacy INTx route status recorded during discovery, if any.
    pub const fn intx_route(&self) -> Option<IntxRouteStatus> {
        self.intx
    }

    /// Firmware node associated with this device at discovery, if any.
    ///
    /// This is the same association the descriptor received at registration;
    /// a probe (including a retry) reads properties and references from this
    /// handle instead of searching firmware again. `None` means discovery
    /// established no association; it does not make the device invalid. The
    /// returned handle is borrowed and remains valid only under the backend's
    /// lifetime contract (boot-static backends today). Querying it follows the
    /// backend's own execution-context rules; holding a handle is not
    /// permission to access hardware or call the backend from an arbitrary
    /// context.
    pub const fn firmware_node(&self) -> Option<FwNode<'static>> {
        self.firmware_node
    }

    /// Find the first MMIO resource.
    pub fn first_mmio(&self) -> Option<crate::MmioRegion> {
        self.resources.iter().find_map(|resource| match resource {
            ResourceDesc::Mmio(mmio) => Some(*mmio),
            _ => None,
        })
    }

    /// Find the first IRQ resource.
    pub fn first_irq(&self) -> Option<crate::IrqResource> {
        self.resources.iter().find_map(|resource| match resource {
            ResourceDesc::Irq(irq) => Some(*irq),
            _ => None,
        })
    }

    /// Current lifecycle state of the object's binding — lock-free atomic
    /// read.
    ///
    /// The atomic is only ever written via [`DeviceState::as_u8`], so an
    /// undecodable value is impossible barring memory corruption; such a value
    /// is treated as [`DeviceState::Removed`] so callers stop operating on the
    /// object instead of panicking on a hot read path.
    pub fn state(&self) -> DeviceState {
        self.binding.state()
    }

    /// Current parent device ID.
    pub fn parent(&self) -> Option<DeviceId> {
        self.relations.lock().parent
    }

    /// Set or clear this object's parent relation.
    ///
    /// Driver-core-internal: parenting is managed by the lifecycle layer
    /// (`attach_device_parent` / removal), never by drivers directly.
    pub(crate) fn set_parent(&self, parent: Option<DeviceId>) {
        self.relations.lock().parent = parent;
    }

    /// Bus instance produced by this device, if any.
    ///
    /// Bus controllers (PCI host bridge, future I2C/USB controllers) report
    /// the `BusId` of the bus they own here so higher-level code can walk
    /// from a controller device to its child bus without scanning every
    /// `BusInstance`.
    pub fn child_bus(&self) -> Option<BusId> {
        self.relations.lock().child_bus
    }

    /// Record (or clear) the bus instance this device produces.
    ///
    /// Intended to be called once during adoption by the backend that owns
    /// the controller. The matching back-link lives on
    /// [`crate::BusInstance::set_controller`].
    pub fn set_child_bus(&self, child_bus: Option<BusId>) {
        self.relations.lock().child_bus = child_bus;
    }

    /// Snapshot direct children.
    pub fn children_snapshot(&self) -> Vec<DeviceId> {
        self.relations.lock().children.clone()
    }

    /// Attach a direct child to this device.
    pub(crate) fn attach_child(&self, child: DeviceId) {
        let mut relations = self.relations.lock();
        if !relations.children.contains(&child) {
            relations.children.push(child);
        }
    }

    /// Detach a direct child from this device.
    pub(crate) fn detach_child(&self, child: DeviceId) {
        let mut relations = self.relations.lock();
        if let Some(pos) = relations
            .children
            .iter()
            .position(|current| *current == child)
        {
            relations.children.swap_remove(pos);
        }
    }

    /// Bound driver ID, if any.
    pub fn driver_id(&self) -> Option<DriverId> {
        self.binding.driver_id()
    }

    /// Bound driver name, if any.
    pub fn driver_name(&self) -> Option<&'static str> {
        self.binding.driver_name()
    }

    /// Device subsystem kind, if known.
    pub fn device_kind(&self) -> Option<DeviceKind> {
        self.binding.device_kind()
    }

    /// Mark the device as matched.
    ///
    /// Driver-core-internal: call via
    /// `lifecycle::dispatch::prepare_device_binding` while the object is still
    /// unpublished.
    pub(crate) fn mark_matched(&self) {
        self.binding.mark_matched();
    }

    /// Bind this device to a driver object.
    ///
    /// Driver-core-internal: call via
    /// `lifecycle::dispatch::prepare_device_binding` while the object is still
    /// unpublished.
    pub(crate) fn bind_driver(
        &self,
        driver_id: DriverId,
        driver_name: &'static str,
        kind: DeviceKind,
    ) {
        self.binding.bind(driver_id, driver_name, kind);
    }

    /// Detach any bound driver metadata.
    ///
    /// Driver-core-internal: invoked by the remove path so a device leaving
    /// the registry never observes a `Bound`/`Active` state with no driver.
    pub(crate) fn detach_driver(&self) {
        self.binding.detach();
    }

    /// Atomically transition into `Removing`. Returns `false` if removal was
    /// already in progress (the lifecycle state is already `Removing` or
    /// `Removed`) or if the device still has outstanding in-use references
    /// handed out by [`DeviceObject::try_acquire`].
    ///
    /// Entering `Removing` only closes admission for new references. If
    /// references are still outstanding, the previous state is restored and no
    /// teardown is committed; the caller may retry later. A `true` return is
    /// the lifecycle layer's gate for irreversible teardown.
    pub(crate) fn begin_removing(&self) -> bool {
        self.binding.begin_removing()
    }

    /// Whether this object is currently in the `Removing` state — lock-free.
    pub fn is_removing(&self) -> bool {
        matches!(self.state(), DeviceState::Removing)
    }

    /// Capture a read-only reference to the current binding instance.
    ///
    /// The returned handle snapshots the bound driver identity and keeps the
    /// binding runtime alive, but does not count as in-use admission and
    /// never blocks removal. Returns `None` when no driver identity is
    /// currently recorded, when the binding is already detached, or when the
    /// capture observes `Removing`/`Removed`.
    ///
    /// The capture is not a publish permission: a successful capture does not
    /// guarantee that a later class publication happens before removal.
    pub fn binding_ref(&self) -> Option<BindingRef> {
        self.binding.capture_ref()
    }

    /// Try to acquire an in-use reference to a live device.
    ///
    /// Returns a [`DeviceUse`] guard only while the device is `Active`. While
    /// any guard is alive the remove path (`DeviceObject::begin_removing`)
    /// refuses to tear the device down, so callers can safely operate on it
    /// for the lifetime of the guard. The guard retains the exact binding
    /// instance admitted here and releases that same instance on drop.
    pub fn try_acquire(self: &Arc<Self>) -> Option<DeviceUse> {
        let binding = Arc::clone(&self.binding);
        if binding.try_acquire() {
            Some(DeviceUse {
                device: Arc::clone(self),
                binding,
            })
        } else {
            None
        }
    }

    /// Register a cleanup callback bound to this device's lifetime ("devres").
    ///
    /// Callbacks run in LIFO order when the device's probe fails or when the
    /// device is removed (see `DeviceObject::run_cleanups`). Drivers use this
    /// to attach resource teardown to the device so neither a failed probe nor
    /// a later removal leaks the resource. Registration order matches
    /// acquisition order, so the most-recently acquired resource is released
    /// first.
    pub fn add_cleanup<F>(&self, cleanup: F)
    where
        F: FnOnce() + Send + 'static,
    {
        self.binding.add_cleanup(Box::new(cleanup));
    }

    /// Run and drain the current cleanup batch in LIFO order.
    ///
    /// Driver-core-internal: invoked on probe failure and by the remove path.
    /// The callbacks are taken out of the lock before running so a callback is
    /// free to touch the device without re-entering the devres lock. A cleanup
    /// registered by a running callback belongs to the next batch; only a call
    /// with no new registrations is a no-op.
    pub(crate) fn run_cleanups(&self) {
        self.binding.run_cleanups();
    }

    /// Mark this device as active.
    ///
    /// Driver-core-internal: call via `lifecycle::dispatch::activate_device`.
    pub(crate) fn mark_active(&self, kind: DeviceKind) {
        self.binding.activate(kind);
    }

    /// Mark this device as removed.
    ///
    /// Driver-core-internal: called by the remove path.
    pub(crate) fn mark_removed(&self) {
        self.binding.mark_removed();
    }

    /// Snapshot this object into metadata form.
    ///
    /// This is a **grouped snapshot**, not a single atomic snapshot of the
    /// whole record: driver metadata is sampled as one group under
    /// `binding.meta`, `parent`/`child_bus` as one relations group, and the
    /// lifecycle state as a separate atomic read taken before the metadata
    /// lock. Each group is internally consistent, but the three samples may
    /// come from different instants. `location/origin/identity/...` are
    /// immutable for the object's lifetime.
    pub fn record_snapshot(&self) -> DeviceRecord {
        let binding = self.binding.snapshot();
        let (parent, child_bus) = {
            let relations = self.relations.lock();
            (relations.parent, relations.child_bus)
        };
        DeviceRecord {
            id: self.id,
            bus_id: self.bus_id,
            parent,
            child_bus,
            location: self.location,
            origin: self.origin,
            identity: self.identity,
            transport: self.transport,
            resources: self.resources.clone(),
            dma: self.dma,
            intx: self.intx,
            driver_name: binding.driver_name,
            driver_id: binding.driver_id,
            device_kind: binding.device_kind,
            state: binding.state,
        }
    }
}

impl core::fmt::Debug for DeviceObject {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DeviceObject")
            .field("id", &self.id)
            .field("bus_id", &self.bus_id)
            .field("location", &self.location)
            .field("origin", &self.origin)
            .field("identity", &self.identity)
            .field("state", &self.state())
            .finish()
    }
}

/// RAII guard representing an outstanding in-use reference to a [`DeviceObject`].
///
/// Created by [`DeviceObject::try_acquire`]. The device cannot be removed while
/// any `DeviceUse` is alive; dropping the guard releases the reference and lets
/// a pending removal proceed.
///
/// # Specific-binding ownership
///
/// The guard stores the binding instance that admitted it and releases that
/// same instance on drop; it does not re-read "the object's current binding".
/// The `Arc<DeviceObject>` is retained only for the public `device()`/`Deref`
/// queries. A read-only [`BindingRef`] keeps the runtime alive without
/// incrementing the count, so only `DeviceUse` guards gate removal.
pub struct DeviceUse {
    device: Arc<DeviceObject>,
    binding: Arc<BindingRuntime>,
}

impl DeviceUse {
    /// Borrow the underlying device object.
    pub fn device(&self) -> &Arc<DeviceObject> {
        &self.device
    }
}

impl Deref for DeviceUse {
    type Target = DeviceObject;

    fn deref(&self) -> &DeviceObject {
        &self.device
    }
}

impl Drop for DeviceUse {
    fn drop(&mut self) {
        self.binding.release_use();
    }
}
