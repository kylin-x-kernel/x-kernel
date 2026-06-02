// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Long-lived device object, equivalent to the kernel's central device fact.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    ops::Deref,
    sync::atomic::{AtomicU8, AtomicUsize, Ordering},
};

use driver_base::DeviceKind;
use kspin::SpinNoPreempt;

use crate::{
    BusId, DeviceId, DeviceIdentity, DeviceLocation, DeviceRecord, DeviceState, DiscoveryOrigin,
    DriverId, ResourceDesc, ResourceSet, TransportInfo,
};

/// Mutable identity fields of a device object protected by a per-object lock.
///
/// The lifecycle state is kept outside this struct in an `AtomicU8` so hot
/// read paths can read it without taking the lock.
struct DeviceObjectState {
    parent: Option<DeviceId>,
    children: Vec<DeviceId>,
    /// Bus instance this device produces, if it is a bus controller / bridge.
    ///
    /// For a PCI host bridge this points at the PCI bus instance whose
    /// endpoints sit beneath the bridge in the device tree. Endpoint and
    /// leaf devices leave this `None`.
    child_bus: Option<BusId>,
    driver_name: Option<&'static str>,
    driver_id: Option<DriverId>,
    device_kind: Option<DeviceKind>,
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
    lifecycle: AtomicU8,
    /// Outstanding in-use references handed out by [`DeviceObject::try_acquire`].
    ///
    /// Removal is gated on this dropping back to zero so a consumer holding a
    /// live reference (an opened handle, an in-flight operation) cannot have
    /// the device torn down underneath it.
    usage: AtomicUsize,
    /// Driver-registered cleanup callbacks ("devres"), run in LIFO order when
    /// a probe fails or the device is removed. Lets a driver attach resource
    /// teardown (free IRQ, release queues, ...) to the device's lifetime so a
    /// failed probe or removal cannot leak it.
    devres: SpinNoPreempt<Vec<Box<dyn FnOnce() + Send>>>,
    state: SpinNoPreempt<DeviceObjectState>,
}

impl DeviceObject {
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
            lifecycle: AtomicU8::new(DeviceState::Discovered.as_u8()),
            usage: AtomicUsize::new(0),
            devres: SpinNoPreempt::new(Vec::new()),
            state: SpinNoPreempt::new(DeviceObjectState {
                parent: None,
                children: Vec::new(),
                child_bus: None,
                driver_name: None,
                driver_id: None,
                device_kind: None,
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

    /// Borrow hardware resources associated with this device.
    pub fn resources(&self) -> &ResourceSet {
        &self.resources
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

    /// Current lifecycle state — lock-free atomic read.
    ///
    /// The atomic is only ever written via [`DeviceState::as_u8`], so an
    /// undecodable value is impossible barring memory corruption; such a value
    /// is treated as [`DeviceState::Removed`] so callers stop operating on the
    /// object instead of panicking on a hot read path.
    pub fn state(&self) -> DeviceState {
        DeviceState::from_u8(self.lifecycle.load(Ordering::Acquire)).unwrap_or(DeviceState::Removed)
    }

    /// Current parent device ID.
    pub fn parent(&self) -> Option<DeviceId> {
        self.state.lock().parent
    }

    /// Set or clear this object's parent relation.
    ///
    /// Driver-core-internal: parenting is managed by the lifecycle layer
    /// (`attach_device_parent` / removal), never by drivers directly.
    pub(crate) fn set_parent(&self, parent: Option<DeviceId>) {
        self.state.lock().parent = parent;
    }

    /// Bus instance produced by this device, if any.
    ///
    /// Bus controllers (PCI host bridge, future I2C/USB controllers) report
    /// the `BusId` of the bus they own here so higher-level code can walk
    /// from a controller device to its child bus without scanning every
    /// `BusInstance`.
    pub fn child_bus(&self) -> Option<BusId> {
        self.state.lock().child_bus
    }

    /// Record (or clear) the bus instance this device produces.
    ///
    /// Intended to be called once during adoption by the backend that owns
    /// the controller. The matching back-link lives on
    /// [`crate::BusInstance::set_controller`].
    pub fn set_child_bus(&self, child_bus: Option<BusId>) {
        self.state.lock().child_bus = child_bus;
    }

    /// Snapshot direct children.
    pub fn children_snapshot(&self) -> Vec<DeviceId> {
        self.state.lock().children.clone()
    }

    /// Attach a direct child to this device.
    pub(crate) fn attach_child(&self, child: DeviceId) {
        let mut state = self.state.lock();
        if !state.children.contains(&child) {
            state.children.push(child);
        }
    }

    /// Detach a direct child from this device.
    pub(crate) fn detach_child(&self, child: DeviceId) {
        let mut state = self.state.lock();
        if let Some(pos) = state.children.iter().position(|current| *current == child) {
            state.children.swap_remove(pos);
        }
    }

    /// Bound driver ID, if any.
    pub fn driver_id(&self) -> Option<DriverId> {
        self.state.lock().driver_id
    }

    /// Bound driver name, if any.
    pub fn driver_name(&self) -> Option<&'static str> {
        self.state.lock().driver_name
    }

    /// Device subsystem kind, if known.
    pub fn device_kind(&self) -> Option<DeviceKind> {
        self.state.lock().device_kind
    }

    /// Store a new lifecycle state with release ordering.
    fn store_state(&self, new_state: DeviceState) {
        self.lifecycle.store(new_state.as_u8(), Ordering::Release);
    }

    /// Mark the device as matched.
    ///
    /// Driver-core-internal: call via `lifecycle::dispatch::mark_device_matched`.
    pub(crate) fn mark_matched(&self) {
        self.store_state(DeviceState::Matched);
    }

    /// Bind this device to a driver object.
    ///
    /// Driver-core-internal: call via `lifecycle::dispatch::bind_device_to_driver`.
    pub(crate) fn bind_driver(
        &self,
        driver_id: DriverId,
        driver_name: &'static str,
        kind: DeviceKind,
    ) {
        {
            let mut state = self.state.lock();
            state.driver_id = Some(driver_id);
            state.driver_name = Some(driver_name);
            state.device_kind = Some(kind);
        }
        self.store_state(DeviceState::Bound);
    }

    /// Detach any bound driver metadata.
    ///
    /// Driver-core-internal: invoked by the remove path so a device leaving
    /// the registry never observes a `Bound`/`Active` state with no driver.
    pub(crate) fn detach_driver(&self) {
        let mut state = self.state.lock();
        state.driver_id = None;
        state.driver_name = None;
        state.device_kind = None;
    }

    /// Atomically transition into `Removing`. Returns `false` if removal was
    /// already in progress (the lifecycle state is already `Removing` or
    /// `Removed`) or if the device still has outstanding in-use references
    /// handed out by [`DeviceObject::try_acquire`].
    pub(crate) fn begin_removing(&self) -> bool {
        // CAS loop allows concurrent readers and other transitions to make
        // progress without holding the per-object spinlock.
        let mut current = self.lifecycle.load(Ordering::Acquire);
        let prior;
        loop {
            // An undecodable discriminant can only mean corruption; treat it as
            // already-removed and decline the transition rather than panicking.
            let Some(cur_state) = DeviceState::from_u8(current) else {
                return false;
            };
            if matches!(cur_state, DeviceState::Removing | DeviceState::Removed) {
                return false;
            }
            match self.lifecycle.compare_exchange_weak(
                current,
                DeviceState::Removing.as_u8(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    prior = cur_state;
                    break;
                }
                Err(observed) => current = observed,
            }
        }

        // The state is now `Removing`, which makes `try_acquire` reject new
        // references. If any are still outstanding we must NOT proceed: no
        // teardown has happened yet (this is the commit point), so it is safe
        // to revert to the prior state and report the device as busy.
        if self.usage.load(Ordering::Acquire) != 0 {
            self.store_state(prior);
            return false;
        }
        true
    }

    /// Whether this object is currently in the `Removing` state — lock-free.
    pub fn is_removing(&self) -> bool {
        matches!(self.state(), DeviceState::Removing)
    }

    /// Try to acquire an in-use reference to a live device.
    ///
    /// Returns a [`DeviceUse`] guard only while the device is `Active`. While
    /// any guard is alive the remove path ([`DeviceObject::begin_removing`])
    /// refuses to tear the device down, so callers can safely operate on it
    /// for the lifetime of the guard.
    pub fn try_acquire(self: &Arc<Self>) -> Option<DeviceUse> {
        // Optimistically take the reference, then verify the device is still
        // usable. Ordering the increment before the state check means a
        // concurrent `begin_removing` either observes our count (and backs
        // off) or has already moved the state out of `Active` (and we back
        // off) — never both proceeding.
        self.usage.fetch_add(1, Ordering::AcqRel);
        if matches!(self.state(), DeviceState::Active) {
            Some(DeviceUse {
                device: self.clone(),
            })
        } else {
            self.usage.fetch_sub(1, Ordering::AcqRel);
            None
        }
    }

    /// Register a cleanup callback bound to this device's lifetime ("devres").
    ///
    /// Callbacks run in LIFO order when the device's probe fails or when the
    /// device is removed (see [`DeviceObject::run_cleanups`]). Drivers use this
    /// to attach resource teardown to the device so neither a failed probe nor
    /// a later removal leaks the resource. Registration order matches
    /// acquisition order, so the most-recently acquired resource is released
    /// first.
    pub fn add_cleanup<F>(&self, cleanup: F)
    where
        F: FnOnce() + Send + 'static,
    {
        self.devres.lock().push(Box::new(cleanup));
    }

    /// Run and drain all registered cleanup callbacks in LIFO order.
    ///
    /// Driver-core-internal: invoked on probe failure and by the remove path.
    /// The callbacks are taken out of the lock before running so a callback is
    /// free to touch the device without re-entering the devres lock.
    pub(crate) fn run_cleanups(&self) {
        let mut cleanups = core::mem::take(&mut *self.devres.lock());
        while let Some(cleanup) = cleanups.pop() {
            cleanup();
        }
    }

    /// Mark this device as active.
    ///
    /// Driver-core-internal: call via `lifecycle::dispatch::activate_device`.
    pub(crate) fn mark_active(&self, kind: DeviceKind) {
        self.state.lock().device_kind = Some(kind);
        self.store_state(DeviceState::Active);
    }

    /// Mark this device as removed.
    ///
    /// Driver-core-internal: called by the remove path.
    pub(crate) fn mark_removed(&self) {
        self.store_state(DeviceState::Removed);
    }

    /// Snapshot this object into metadata form.
    pub fn record_snapshot(&self) -> DeviceRecord {
        // Read lifecycle outside the per-object lock to keep the critical
        // section minimal.
        let lifecycle = self.state();
        let state = self.state.lock();
        DeviceRecord {
            id: self.id,
            bus_id: self.bus_id,
            parent: state.parent,
            child_bus: state.child_bus,
            location: self.location,
            origin: self.origin,
            identity: self.identity,
            transport: self.transport,
            resources: self.resources.clone(),
            driver_name: state.driver_name,
            driver_id: state.driver_id,
            device_kind: state.device_kind,
            state: lifecycle,
        }
    }
}

impl device_res::DeviceResource for DeviceObject {
    fn resources(&self) -> &[ResourceDesc] {
        // Inherent `DeviceObject::resources` takes priority in method
        // resolution, so this calls the inherent accessor (not itself).
        self.resources.as_slice()
    }

    fn register_cleanup(&self, cleanup: Box<dyn FnOnce() + Send>) {
        self.add_cleanup(cleanup);
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
pub struct DeviceUse {
    device: Arc<DeviceObject>,
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
        self.device.usage.fetch_sub(1, Ordering::AcqRel);
    }
}
