// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Long-lived device object, equivalent to the kernel's central device fact.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::ops::Deref;

use driver_base::{DeviceKind, DriverError, DriverResult};
use driver_os_api::DmaInfo;
use fwnode::FwNode;
use kmutex::Mutex;
use kspin::{SpinNoPreempt, SpinNoPreemptGuard};

use super::binding::{BindingRef, BindingRuntime, BindingSource, DataInstallError, DataTakeError};
use crate::{
    BusId, DeviceId, DeviceIdentity, DeviceLocation, DeviceRecord, DeviceState, DiscoveryOrigin,
    DriverId, IntxRouteStatus, ResourceDesc, ResourceSet, TransportInfo, driver::DriverData,
};

/// Topology relations of a device object protected by a per-object lock.
///
/// These are device facts managed by the lifecycle layer, not by the binding
/// runtime: `BindingRuntime` does not own or clear them. Registration creates
/// them, unbind preserves them, and unregister conditionally removes them.
pub(crate) struct DeviceRelations {
    pub(crate) parent: Option<DeviceId>,
    pub(crate) children: Vec<DeviceId>,
    /// Bus instance this device produces, if it is a bus controller / bridge.
    ///
    /// For a PCI host bridge this points at the PCI bus instance whose
    /// endpoints sit beneath the bridge in the device tree. Endpoint and
    /// leaf devices leave this `None`.
    pub(crate) child_bus: Option<BusId>,
}

/// Whether the stable object still belongs to a registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Registration {
    /// Test/fixture object that never had a descriptor registration.
    #[cfg_attr(
        all(not(unittest), not(doc)),
        expect(
            dead_code,
            reason = "CG-024-lint-exceptions: constructed only by unit-test fixtures"
        )
    )]
    Detached,
    /// Registered by `device_desc_add`/`adopt_active_device`.
    Registered,
    /// Unregistered; terminal and cannot install a new binding.
    Ended,
}

/// Replaceable current-binding slot of a stable device object.
struct BindingSlot {
    registration: Registration,
    current: Option<Arc<BindingRuntime>>,
}

/// Long-lived device object owned by the driver core.
///
/// The object and its [`DeviceId`] survive probe failures, unbind and rebind.
/// Discovery facts are immutable; only the current binding slot is replaced.
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
    /// Descriptor binding lock shared with the registry entry.
    ///
    /// Lifecycle writes (install/clear binding, add cleanup, topology entry
    /// points) verify that the caller owns this lock for a registered object.
    binding_lock: Arc<Mutex<()>>,
    /// Registration membership plus the current binding instance, if any.
    ///
    /// The slot spinlock is a leaf: it is taken after the descriptor binding
    /// lock and never held while re-entering the registry.
    slot: SpinNoPreempt<BindingSlot>,
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
        device.install_bound_for_test(DriverId::new(0), "test-driver", kind);
        device
    }

    /// Drive a detached fixture's current binding to `Removed`.
    ///
    /// Test-only: class-registry tests need a terminal binding instance without
    /// running the whole probe/remove path. Does nothing when removal is
    /// already in progress or a `DeviceUse` is outstanding.
    #[cfg(unittest)]
    pub fn force_active_for_test(&self) {
        let Some(kind) = self.device_kind() else {
            return;
        };
        let slot = self.slot.lock();
        if let Some(runtime) = slot.current.as_ref() {
            runtime.activate(kind);
        }
    }

    #[cfg(unittest)]
    pub fn force_removed_for_test(&self) {
        if self.begin_removing() {
            self.mark_removed();
        }
    }

    /// Install one bound runtime on a detached fixture.
    ///
    /// Test-only: lets fixtures keep their custom bus/location facts and still
    /// exercise binding lifecycle. No registration, event or hardware resource
    /// is created. The fixture stays `Detached`, so lifecycle writes skip the
    /// descriptor-lock ownership check.
    #[cfg(unittest)]
    pub fn install_bound_for_test(
        &self,
        driver_id: DriverId,
        driver_name: &'static str,
        kind: DeviceKind,
    ) {
        let runtime = Arc::new(BindingRuntime::new(BindingSource::Test));
        runtime.bind(driver_id, driver_name, kind);
        *self.slot.lock() = BindingSlot {
            registration: Registration::Detached,
            current: Some(runtime),
        };
    }

    /// Create a detached device fixture object.
    ///
    /// Test-only: this constructor has no descriptor registration and no
    /// lifecycle write-context check. Production registration uses
    /// [`DeviceObject::new_registered`].
    #[cfg(unittest)]
    pub fn new(
        id: DeviceId,
        bus_id: BusId,
        location: DeviceLocation,
        origin: DiscoveryOrigin,
        identity: DeviceIdentity,
        transport: Option<TransportInfo>,
        resources: ResourceSet,
    ) -> Self {
        Self::with_binding_lock(
            id,
            bus_id,
            location,
            origin,
            identity,
            transport,
            resources,
            Arc::new(Mutex::new(())),
            Registration::Detached,
        )
    }

    /// Create the stable object for a new descriptor registration.
    ///
    /// The object starts registered (`Discovered`) with no current binding and
    /// shares `binding_lock` with its descriptor entry.
    #[expect(
        clippy::too_many_arguments,
        reason = "CG-024-lint-exceptions: internal constructor mirrors the immutable discovery \
                  facts and registration metadata; a builder would duplicate the same fields \
                  without adding invariants, and only the descriptor registration and adoption \
                  paths call it"
    )]
    pub(crate) fn new_registered(
        id: DeviceId,
        bus_id: BusId,
        location: DeviceLocation,
        origin: DiscoveryOrigin,
        identity: DeviceIdentity,
        transport: Option<TransportInfo>,
        resources: ResourceSet,
        binding_lock: Arc<Mutex<()>>,
    ) -> Self {
        Self::with_binding_lock(
            id,
            bus_id,
            location,
            origin,
            identity,
            transport,
            resources,
            binding_lock,
            Registration::Registered,
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "Internal constructor mirrors the immutable discovery facts and registration \
                  metadata; it is called only by the two public constructors above"
    )]
    fn with_binding_lock(
        id: DeviceId,
        bus_id: BusId,
        location: DeviceLocation,
        origin: DiscoveryOrigin,
        identity: DeviceIdentity,
        transport: Option<TransportInfo>,
        resources: ResourceSet,
        binding_lock: Arc<Mutex<()>>,
        registration: Registration,
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
            binding_lock,
            slot: SpinNoPreempt::new(BindingSlot {
                registration,
                current: None,
            }),
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
    /// builds the stable object from its descriptor. There is no
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

    /// Projected lifecycle state of the registration and current binding.
    ///
    /// - unregistered object -> [`DeviceState::Removed`];
    /// - registered object with no current binding -> [`DeviceState::Discovered`];
    /// - otherwise the current binding runtime's state.
    ///
    /// The projection takes the slot spinlock. It is a point-in-time view; a
    /// concurrent install/clear may change the answer immediately afterwards.
    pub fn state(&self) -> DeviceState {
        let slot = self.slot.lock();
        match slot.registration {
            Registration::Ended => DeviceState::Removed,
            _ => match &slot.current {
                Some(runtime) => runtime.state(),
                None => DeviceState::Discovered,
            },
        }
    }

    /// Whether the object still belongs to a descriptor registration.
    pub fn is_registered(&self) -> bool {
        matches!(self.slot.lock().registration, Registration::Registered)
    }

    /// Current parent device ID.
    pub fn parent(&self) -> Option<DeviceId> {
        self.relations.lock().parent
    }

    /// Set or clear this object's parent relation.
    ///
    /// Test-only topology helper. It acquires this object's relations lock
    /// internally; the caller must not already hold it. Production topology
    /// code uses [`DeviceObject::with_relations_locks`] and writes the guard.
    #[cfg(unittest)]
    pub(crate) fn set_parent(&self, parent: Option<DeviceId>) {
        self.relations.lock().parent = parent;
    }

    /// Clear the parent only if it still equals `expected`.
    ///
    /// This is the only safe form for delayed cleanup: it cannot erase a newer
    /// parent relation installed after the cleanup snapshot.
    pub(crate) fn clear_parent_if(&self, expected: DeviceId) -> bool {
        let mut relations = self.relations.lock();
        if relations.parent == Some(expected) {
            relations.parent = None;
            true
        } else {
            false
        }
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

    /// Snapshot the parent and direct children together.
    pub(crate) fn topology_snapshot(&self) -> (Option<DeviceId>, Vec<DeviceId>) {
        let relations = self.relations.lock();
        (relations.parent, relations.children.clone())
    }

    /// Attach a direct child to this device.
    ///
    /// Test-only topology helper. It acquires this object's relations lock
    /// internally; the caller must not already hold it.
    #[cfg(unittest)]
    pub(crate) fn attach_child(&self, child: DeviceId) {
        let mut relations = self.relations.lock();
        if !relations.children.contains(&child) {
            relations.children.push(child);
        }
    }

    /// Detach a direct child from this device.
    ///
    /// This helper acquires this object's relations lock internally; the
    /// caller must not already hold it. Ordered multi-node writers must update
    /// the guard directly inside [`DeviceObject::with_relations_locks`].
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

    /// Acquire the relations locks of `nodes` once, in the given ID order.
    ///
    /// `nodes` must be sorted by ascending, unique [`DeviceId`]. The callback
    /// receives one guard per node in the same order. This is the only helper
    /// that takes more than one relations lock; taking all of them in a single
    /// total order is what prevents parent/child role-order ABBA deadlocks.
    pub(crate) fn with_relations_locks<R>(
        nodes: &[Arc<DeviceObject>],
        f: impl FnOnce(&mut [SpinNoPreemptGuard<'_, DeviceRelations>]) -> R,
    ) -> R {
        debug_assert!(nodes.windows(2).all(|pair| pair[0].id() < pair[1].id()));
        let mut guards = Vec::with_capacity(nodes.len());
        for node in nodes {
            guards.push(node.relations.lock());
        }
        #[cfg(unittest)]
        {
            let ids: Vec<DeviceId> = nodes.iter().map(|node| node.id()).collect();
            crate::lifecycle::test_hooks::relations_locked(&ids);
        }
        f(&mut guards)
    }

    /// Bound driver ID of the current binding, if any.
    pub fn driver_id(&self) -> Option<DriverId> {
        self.current_runtime()
            .and_then(|runtime| runtime.driver_id())
    }

    /// Bound driver name of the current binding, if any.
    pub fn driver_name(&self) -> Option<&'static str> {
        self.current_runtime()
            .and_then(|runtime| runtime.driver_name())
    }

    /// Device subsystem kind of the current binding, if known.
    pub fn device_kind(&self) -> Option<DeviceKind> {
        self.current_runtime()
            .and_then(|runtime| runtime.device_kind())
    }

    /// Borrow the current runtime, if the slot has one.
    fn current_runtime(&self) -> Option<Arc<BindingRuntime>> {
        self.slot.lock().current.clone()
    }

    /// Mark the current binding as matched.
    ///
    /// Driver-core-internal: called via
    /// `lifecycle::dispatch::prepare_device_binding` inside the lifecycle
    /// write context.
    pub(crate) fn mark_matched(&self) {
        if let Some(runtime) = self.current_runtime() {
            runtime.mark_matched();
        }
    }

    /// Bind the current runtime to a driver object.
    ///
    /// Driver-core-internal: called via
    /// `lifecycle::dispatch::prepare_device_binding` inside the lifecycle
    /// write context.
    pub(crate) fn bind_driver(
        &self,
        driver_id: DriverId,
        driver_name: &'static str,
        kind: DeviceKind,
    ) {
        if let Some(runtime) = self.current_runtime() {
            runtime.bind(driver_id, driver_name, kind);
        }
    }

    /// Detach any bound driver metadata from the current binding.
    ///
    /// Driver-core-internal: invoked by the remove path so a retired binding
    /// never observes `Bound`/`Active` state with stale identity metadata.
    pub(crate) fn detach_driver(&self) {
        if let Some(runtime) = self.current_runtime() {
            runtime.detach();
        }
    }

    /// Atomically transition the current binding into `Removing`.
    ///
    /// Returns `false` when there is no current binding, removal is already in
    /// progress, or [`crate::DeviceUse`] guards are outstanding. In every
    /// failure case the current binding and its resources are unchanged; the
    /// caller may retry.
    pub(crate) fn begin_removing(&self) -> bool {
        self.current_runtime()
            .is_some_and(|runtime| runtime.begin_removing())
    }

    /// Whether the current binding is in `Removing` — projected state read.
    pub fn is_removing(&self) -> bool {
        matches!(self.state(), DeviceState::Removing)
    }

    /// Capture a read-only reference to the current binding instance.
    ///
    /// The returned handle snapshots the bound driver identity and keeps the
    /// binding runtime alive, but does not count as in-use admission and
    /// never blocks removal. Returns `None` when there is no current binding
    /// or when capture observes `Removing`/`Removed`.
    ///
    /// The capture is not a publish permission: a successful capture does not
    /// guarantee that a later class publication happens before removal.
    pub fn binding_ref(&self) -> Option<BindingRef> {
        let runtime = self.slot.lock().current.clone()?;
        runtime.capture_ref()
    }

    fn capture_lifecycle_binding(&self) -> Option<BindingRef> {
        let runtime = {
            let slot = self.slot.lock();
            match slot.registration {
                Registration::Ended => return None,
                Registration::Registered if !self.binding_lock.is_owned_by_current() => {
                    return None;
                }
                Registration::Detached | Registration::Registered => slot.current.clone()?,
            }
        };
        runtime.capture_ref()
    }

    /// Capture the current binding for a lifecycle write operation.
    ///
    /// Production visibility is crate-private: only the lifecycle framework
    /// may mint a write context. Test builds expose the helper so existing
    /// fixtures can construct a `ProbeContext`; PM/remove consumers cannot use
    /// it to bypass the phase permission.
    #[cfg(not(unittest))]
    pub(crate) fn binding_ref_for_lifecycle_write(&self) -> Option<BindingRef> {
        self.capture_lifecycle_binding()
    }

    /// Test-build compatibility helper; see the production cfg above.
    #[cfg(unittest)]
    pub fn binding_ref_for_lifecycle_write(&self) -> Option<BindingRef> {
        self.capture_lifecycle_binding()
    }

    /// Capture the exact current binding for the remove path. Unlike
    /// [`Self::binding_ref_for_lifecycle_write`], a `Removing` instance is
    /// valid.
    pub(crate) fn binding_ref_for_removal(&self) -> Option<BindingRef> {
        let slot = self.slot.lock();
        match slot.registration {
            Registration::Ended => return None,
            Registration::Registered if !self.binding_lock.is_owned_by_current() => return None,
            Registration::Detached | Registration::Registered => {}
        }
        let runtime = slot.current.clone()?;
        drop(slot);
        runtime.capture_ref_for_removal()
    }

    /// Install probe-owned DriverData into the exact runtime.
    pub(crate) fn install_data(
        &self,
        runtime: &Arc<BindingRuntime>,
        data: DriverData,
    ) -> Result<(), DataInstallError> {
        #[cfg(unittest)]
        if super::binding::take_data_install_failure(self.id) {
            return Err(DataInstallError::new(
                super::binding::DataInstallFailureKind::Injected,
                DriverError::BadState,
                data,
            ));
        }
        let Some(current) = self.current_runtime() else {
            return Err(DataInstallError::new(
                super::binding::DataInstallFailureKind::RuntimeMismatch,
                DriverError::BadState,
                data,
            ));
        };
        if !Arc::ptr_eq(&current, runtime) {
            return Err(DataInstallError::new(
                super::binding::DataInstallFailureKind::RuntimeMismatch,
                DriverError::BadState,
                data,
            ));
        }
        current.install_data(data)
    }

    /// Take DriverData from the exact runtime.
    pub(crate) fn take_data(
        &self,
        runtime: &Arc<BindingRuntime>,
    ) -> Result<Option<DriverData>, DataTakeError> {
        let Some(current) = self.current_runtime() else {
            return Err(DataTakeError::RuntimeMismatch);
        };
        if !Arc::ptr_eq(&current, runtime) {
            return Err(DataTakeError::RuntimeMismatch);
        }
        current.take_data()
    }

    /// Return PM-taken DriverData to the exact runtime.
    pub(crate) fn restore_data(
        &self,
        runtime: &Arc<BindingRuntime>,
        data: DriverData,
    ) -> Result<(), DataInstallError> {
        let Some(current) = self.current_runtime() else {
            return Err(DataInstallError::new(
                super::binding::DataInstallFailureKind::RuntimeMismatch,
                DriverError::BadState,
                data,
            ));
        };
        if !Arc::ptr_eq(&current, runtime) {
            return Err(DataInstallError::new(
                super::binding::DataInstallFailureKind::RuntimeMismatch,
                DriverError::BadState,
                data,
            ));
        }
        current.restore_data(data)
    }

    /// Install a brand-new binding runtime in the empty slot.
    ///
    /// The runtime is always freshly allocated; this API cannot reuse or
    /// reset a retired instance. The caller must hold the descriptor binding
    /// lock, and the object must be registered with no current binding.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError::BadState`] when the object is not registered,
    /// the slot is not empty, or the caller is not the descriptor-lock owner.
    pub(crate) fn install_fresh_binding(&self, source: BindingSource) -> DriverResult<()> {
        let runtime = Arc::new(BindingRuntime::new(source));
        let mut slot = self.slot.lock();
        if slot.registration != Registration::Registered {
            return Err(DriverError::BadState);
        }
        if slot.current.is_some() || !self.binding_lock.is_owned_by_current() {
            return Err(DriverError::BadState);
        }
        slot.current = Some(runtime);
        Ok(())
    }

    /// Remove the current binding from the slot without resetting it.
    ///
    /// Driver-core-internal: called only after the old binding has reached its
    /// terminal state and its cleanup/event path completed.
    pub(crate) fn clear_binding(&self) -> Option<Arc<BindingRuntime>> {
        self.slot.lock().current.take()
    }

    /// Terminate the registration and remove any current binding.
    ///
    /// Driver-core-internal: called by unregister after teardown. The object
    /// can never install a binding again.
    pub(crate) fn mark_unregistered(&self) {
        let mut slot = self.slot.lock();
        slot.registration = Registration::Ended;
        slot.current = None;
    }

    /// Exact-instance admission used by [`BindingRef::try_acquire`].
    pub(super) fn try_acquire_exact(
        self: &Arc<Self>,
        runtime: &Arc<BindingRuntime>,
    ) -> Option<DeviceUse> {
        let slot = self.slot.lock();
        if !slot
            .current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, runtime))
        {
            return None;
        }
        if !runtime.try_acquire() {
            return None;
        }
        Some(DeviceUse {
            device: Arc::clone(self),
            binding: Arc::clone(runtime),
        })
    }

    /// Exact-instance cleanup registration used by [`BindingRef::add_cleanup`].
    pub(super) fn add_cleanup_exact(
        &self,
        runtime: &Arc<BindingRuntime>,
        cleanup: Box<dyn FnOnce() + Send>,
    ) -> DriverResult<()> {
        let slot = self.slot.lock();
        if !slot
            .current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, runtime))
        {
            return Err(DriverError::BadState);
        }
        if slot.registration == Registration::Registered && !self.binding_lock.is_owned_by_current()
        {
            return Err(DriverError::BadState);
        }
        drop(slot);
        if matches!(
            runtime.state(),
            DeviceState::Removing | DeviceState::Removed
        ) {
            return Err(DriverError::BadState);
        }
        runtime.add_cleanup(cleanup);
        Ok(())
    }

    /// Run and drain the current binding's cleanup batch in LIFO order.
    ///
    /// Driver-core-internal: invoked on probe failure and by the remove path
    /// while holding the descriptor binding lock. The callbacks are taken out
    /// of the lock before running.
    pub(crate) fn run_cleanups(&self) {
        if let Some(runtime) = self.current_runtime() {
            runtime.run_cleanups();
        }
    }

    /// Mark the current binding as active.
    ///
    /// Driver-core-internal: called via `lifecycle::dispatch::activate_device`.
    pub(crate) fn mark_active(&self, kind: DeviceKind) {
        if let Some(runtime) = self.current_runtime() {
            runtime.activate(kind);
        }
    }

    /// Mark the current binding as removed (terminal).
    ///
    /// Driver-core-internal: called by the probe-failure and remove paths.
    pub(crate) fn mark_removed(&self) {
        if let Some(runtime) = self.current_runtime() {
            runtime.mark_removed();
        }
    }

    /// Snapshot this object into metadata form.
    ///
    /// This is a **grouped snapshot**, not a single atomic snapshot of the
    /// whole record: the slot is sampled once to project registration state
    /// and capture the current runtime, then the runtime's driver metadata and
    /// parent/child_bus relations are sampled separately. Each group is
    /// internally consistent, but the groups may come from different
    /// instants. `location/origin/identity/...` are immutable.
    pub fn record_snapshot(&self) -> DeviceRecord {
        let (state, binding) = {
            let slot = self.slot.lock();
            let state = match slot.registration {
                Registration::Ended => DeviceState::Removed,
                _ => DeviceState::Discovered,
            };
            (state, slot.current.clone())
        };
        let binding = match binding {
            Some(runtime) => runtime.snapshot(),
            None => super::binding::BindingSnapshot {
                state,
                driver_id: None,
                driver_name: None,
                device_kind: None,
            },
        };
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
/// Created by [`BindingRef::try_acquire`]. The device cannot be removed while
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
