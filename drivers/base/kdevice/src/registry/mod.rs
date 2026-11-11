// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Short-lock global object index and ID allocator.

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};

use kmutex::Mutex;
use kspin::{SpinNoPreempt, SpinNoPreemptGuard};
use lazyinit::LazyInit;

use crate::{
    BusId, BusInfo, BusInstance, BusTypeId, BusTypeObject, DeviceDesc, DeviceDescId,
    DeviceEventKind, DeviceId, DeviceObject, DeviceRecord, DriverId, DriverInfo, DriverObject,
    FirmwareMatchSpec,
    lifecycle::{event::DeviceEventCallback, subscribers::DeviceEventSubscribers},
};

static DEVICE_REGISTRY: LazyInit<SpinNoPreempt<DeviceRegistry>> = LazyInit::new();

// ID allocators are kept outside the registry lock so callers can mint a
// fresh ID without contending against snapshot/iteration paths. The counters
// are monotonically increasing and only need `fetch_add` semantics, so
// `Relaxed` ordering is sufficient. All four use a 64-bit width, which is wide
// enough that exhaustion (and the associated overflow panic) is unreachable in
// practice even under sustained hotplug churn with non-reused IDs.
static NEXT_BUS_ID: AtomicU64 = AtomicU64::new(0);
static NEXT_DRIVER_ID: AtomicU64 = AtomicU64::new(0);
static NEXT_DESC_ID: AtomicU64 = AtomicU64::new(0);
static NEXT_DEVICE_ID: AtomicU64 = AtomicU64::new(0);

#[expect(
    clippy::panic,
    reason = "CG-021-no-panic-on-reachable-failure: Monotonic 64-bit ID counters cannot reach \
              u64::MAX through ID allocation in practice, as the counter comment explains"
)]
fn fetch_add_u64(counter: &AtomicU64, what: &'static str) -> u64 {
    let prev = counter.fetch_add(1, Ordering::Relaxed);
    if prev == u64::MAX {
        panic!("DeviceRegistry: {} u64 counter overflow", what);
    }
    prev
}

/// Descriptor-first lifecycle state tracked before or alongside a live object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeviceDescState {
    /// The descriptor has not yet been claimed by a successful probe.
    Pending,
    /// A probe is currently evaluating this descriptor.
    Probing,
    /// A live device object was published from this descriptor.
    Bound(DeviceId),
}

struct DeviceDescEntry {
    desc: DeviceDesc,
    state: DeviceDescState,
    /// Serializes binding attempts for this descriptor.
    ///
    /// The object is stable across candidate rebuilds, probe failures and
    /// requeue/remove, so every path that can bind or unbind this descriptor
    /// contends on the same lock. It is deliberately not stored on
    /// `DeviceObject`: a failed candidate's object is discarded, while the
    /// descriptor outlives it. The registry only clones the `Arc`; blocking
    /// acquisition happens outside the registry spinlock.
    binding: Arc<Mutex<()>>,
}

impl DeviceDescEntry {
    fn pending(desc: DeviceDesc, binding: Arc<Mutex<()>>) -> Self {
        Self {
            desc,
            state: DeviceDescState::Pending,
            binding,
        }
    }
}

/// Descriptor ID plus its binding lock handle.
///
/// Used by paths that snapshot descriptors under the registry lock and must
/// then acquire each descriptor's binding lock outside it.
pub(crate) struct DescBindingRef {
    pub id: DeviceDescId,
    pub binding: Arc<Mutex<()>>,
}

/// Short-lock index for long-lived driver-core objects.
///
/// By-id collections use `BTreeMap` so lookups, inserts, and removals are
/// `O(log n)` instead of the previous linear scans; iteration order follows
/// the monotonically increasing IDs, i.e. registration order. `bus_types`
/// stays a `Vec` because it holds only a handful of fixed entries keyed by a
/// string `BusTypeId`.
pub(crate) struct DeviceRegistry {
    descriptors: BTreeMap<DeviceDescId, DeviceDescEntry>,
    devices: BTreeMap<DeviceId, Arc<DeviceObject>>,
    buses: BTreeMap<BusId, Arc<BusInstance>>,
    bus_types: Vec<Arc<BusTypeObject>>,
    drivers: BTreeMap<DriverId, Arc<DriverObject>>,
    subscribers: DeviceEventSubscribers,
}

impl DeviceRegistry {
    fn new() -> Self {
        Self {
            descriptors: BTreeMap::new(),
            devices: BTreeMap::new(),
            buses: BTreeMap::new(),
            bus_types: Vec::new(),
            drivers: BTreeMap::new(),
            subscribers: DeviceEventSubscribers::new(),
        }
    }

    /// Allocate a unique bus ID.
    pub(crate) fn alloc_bus_id(&mut self) -> BusId {
        BusId::new(fetch_add_u64(&NEXT_BUS_ID, "BusId"))
    }

    /// Allocate a unique driver ID.
    pub(crate) fn alloc_driver_id(&mut self) -> DriverId {
        DriverId::new(fetch_add_u64(&NEXT_DRIVER_ID, "DriverId"))
    }

    /// Allocate a unique discovery descriptor ID.
    pub(crate) fn alloc_device_desc_id(&mut self) -> DeviceDescId {
        DeviceDescId::new(fetch_add_u64(&NEXT_DESC_ID, "DeviceDescId"))
    }

    /// Allocate a unique device ID.
    pub(crate) fn alloc_device_id(&mut self) -> DeviceId {
        DeviceId::new(fetch_add_u64(&NEXT_DEVICE_ID, "DeviceId"))
    }

    /// Find a discovery descriptor.
    pub(crate) fn find_device_desc(&self, id: DeviceDescId) -> Option<DeviceDesc> {
        self.descriptors.get(&id).map(|entry| entry.desc.clone())
    }

    /// Clone the binding lock handle for a descriptor.
    ///
    /// Callers acquire the returned lock outside the registry lock.
    pub(crate) fn binding_for_desc(&self, id: DeviceDescId) -> Option<Arc<Mutex<()>>> {
        self.descriptors.get(&id).map(|entry| entry.binding.clone())
    }

    /// Snapshot descriptors that still need a binding attempt on one of
    /// `bus_types`.
    ///
    /// Includes both `Pending` and `Probing` descriptors. A descriptor that is
    /// being bound right now must not be omitted: the scanner acquires its
    /// binding lock, waits for the current attempt, and rechecks the state.
    /// Collecting the IDs and lock handles under the registry lock is safe;
    /// blocking on them is not.
    pub(crate) fn descriptors_awaiting_binding(
        &self,
        bus_types: &[BusTypeId],
    ) -> Vec<DescBindingRef> {
        self.descriptors
            .iter()
            .filter(|(_, entry)| {
                matches!(
                    entry.state,
                    DeviceDescState::Pending | DeviceDescState::Probing
                )
            })
            .filter(|(_, entry)| {
                self.find_bus(entry.desc.bus_id())
                    .is_some_and(|bus| bus_types.contains(&bus.bus_type()))
            })
            .map(|(id, entry)| DescBindingRef {
                id: *id,
                binding: entry.binding.clone(),
            })
            .collect()
    }

    /// Return the descriptor-first lifecycle state.
    pub(crate) fn device_desc_state(&self, id: DeviceDescId) -> Option<DeviceDescState> {
        self.descriptors.get(&id).map(|entry| entry.state)
    }

    /// Find the live device currently created from a descriptor.
    pub(crate) fn find_device_for_desc(&self, id: DeviceDescId) -> Option<Arc<DeviceObject>> {
        let device_id = match self.descriptors.get(&id)?.state {
            DeviceDescState::Bound(device_id) => device_id,
            _ => return None,
        };
        self.find_device(device_id)
    }

    /// Find a device object.
    pub(crate) fn find_device(&self, id: DeviceId) -> Option<Arc<DeviceObject>> {
        self.devices.get(&id).cloned()
    }

    /// Register a descriptor with the binding lock object that will serialize
    /// every attempt to bind it.
    ///
    /// The caller constructs `binding` before taking the registry lock; the
    /// lock is never created or acquired here. Re-registering an existing ID
    /// refreshes the descriptor copy but keeps the existing lock object and
    /// lifecycle state.
    pub(crate) fn add_device_desc_with_binding(
        &mut self,
        desc: DeviceDesc,
        binding: Arc<Mutex<()>>,
    ) {
        match self.descriptors.get_mut(&desc.id()) {
            Some(existing) => existing.desc = desc,
            None => {
                let id = desc.id();
                self.descriptors
                    .insert(id, DeviceDescEntry::pending(desc, binding));
            }
        }
    }

    /// Claim a `Pending` descriptor for a binding attempt.
    ///
    /// The caller must already hold the descriptor's binding lock. Moving the
    /// state to `Probing` is only valid for the lock holder, which is what
    /// makes the state safe to re-read after waiting.
    pub(crate) fn mark_device_desc_probing(&mut self, id: DeviceDescId) -> Option<DeviceDesc> {
        let entry = self.descriptors.get_mut(&id)?;
        if entry.state != DeviceDescState::Pending {
            return None;
        }
        entry.state = DeviceDescState::Probing;
        Some(entry.desc.clone())
    }

    pub(crate) fn requeue_device_desc(&mut self, id: DeviceDescId) {
        if let Some(entry) = self.descriptors.get_mut(&id)
            && entry.state == DeviceDescState::Probing
        {
            entry.state = DeviceDescState::Pending;
        }
    }

    /// Restore `Pending` only if the descriptor is still bound to
    /// `device_id`.
    ///
    /// The caller holds the descriptor's binding lock and rechecks after any
    /// wait, so this cannot requeue a descriptor that another path already
    /// rebound to a newer device.
    pub(crate) fn requeue_device_desc_if_bound(
        &mut self,
        id: DeviceDescId,
        device_id: DeviceId,
    ) -> bool {
        match self.descriptors.get_mut(&id) {
            Some(entry) if entry.state == DeviceDescState::Bound(device_id) => {
                entry.state = DeviceDescState::Pending;
                true
            }
            _ => false,
        }
    }

    /// Remove a descriptor only if it is still in `expected`.
    ///
    /// The caller holds the descriptor's binding lock, so no binding-state
    /// transition can race this check-and-delete. Returning `None` means the
    /// state did not match (or the entry was already gone); callers treat that
    /// as a broken invariant, not as a normal race.
    pub(crate) fn remove_device_desc_if_state(
        &mut self,
        id: DeviceDescId,
        expected: DeviceDescState,
    ) -> Option<DeviceDesc> {
        match self.descriptors.get(&id) {
            Some(entry) if entry.state == expected => {
                self.descriptors.remove(&id).map(|entry| entry.desc)
            }
            _ => None,
        }
    }

    /// Whether `id` is still bound to exactly `device_id`.
    pub(crate) fn device_desc_is_bound_to(&self, id: DeviceDescId, device_id: DeviceId) -> bool {
        matches!(
            self.descriptors.get(&id).map(|entry| entry.state),
            Some(DeviceDescState::Bound(bound)) if bound == device_id
        )
    }

    /// Return the unique descriptor currently bound to `device_id`.
    ///
    /// Exactly one descriptor must own a published device. Zero or multiple
    /// matches mean the 1:1 invariant is broken; callers treat that as
    /// `BadState` and must not perform partial teardown.
    pub(crate) fn descriptor_bound_to_device(&self, device_id: DeviceId) -> Option<DescBindingRef> {
        let mut found = None;
        for (id, entry) in &self.descriptors {
            if entry.state == DeviceDescState::Bound(device_id) {
                if found.is_some() {
                    return None;
                }
                found = Some(DescBindingRef {
                    id: *id,
                    binding: entry.binding.clone(),
                });
            }
        }
        found
    }

    pub(crate) fn bind_device_desc(&mut self, desc_id: DeviceDescId, device_id: DeviceId) {
        if let Some(entry) = self.descriptors.get_mut(&desc_id) {
            entry.state = DeviceDescState::Bound(device_id);
        }
    }

    /// Clear any descriptor parent linkage that still points at a device that
    /// has been removed, so a later reprobe does not attach under a stale
    /// parent id.
    pub(crate) fn clear_descriptor_parents_for(&mut self, removed: DeviceId) {
        for entry in self.descriptors.values_mut() {
            if entry.desc.parent() == Some(removed) {
                entry.desc.clear_parent();
            }
        }
    }

    /// Find a bus object.
    pub(crate) fn find_bus(&self, id: BusId) -> Option<Arc<BusInstance>> {
        self.buses.get(&id).cloned()
    }

    /// Find a bus instance by its human-readable name.
    ///
    /// Used by backends that need to cross-link a controller they own with
    /// the upstream bus that hosts them (e.g. the PCI host bridge living on
    /// a `platform-firmware` or `platform-static` bus).
    pub(crate) fn find_bus_by_name(&self, name: &str) -> Option<Arc<BusInstance>> {
        self.buses
            .values()
            .find(|bus| bus.info().name == name)
            .cloned()
    }

    /// Find a driver object.
    pub(crate) fn find_driver(&self, id: DriverId) -> Option<Arc<DriverObject>> {
        self.drivers.get(&id).cloned()
    }

    /// Find a driver object by its stable name.
    pub(crate) fn find_driver_by_name(&self, name: &str) -> Option<Arc<DriverObject>> {
        self.drivers
            .values()
            .find(|driver| driver.name() == name)
            .cloned()
    }

    /// Register a bus type object with a custom matcher.
    pub(crate) fn register_bus_type(&mut self, bus_type: Arc<BusTypeObject>) {
        if !self.bus_types.iter().any(|bt| bt.id() == bus_type.id()) {
            self.bus_types.push(bus_type);
        }
    }

    /// Find a previously registered bus type object.
    ///
    /// # Panics
    ///
    /// Panics if the bus type was not registered before use. Call
    /// [`register_bus_type`] during init before creating bus instances.
    #[expect(
        clippy::panic,
        reason = "CG-021-no-panic-on-reachable-failure: Driver-core init (default_bus_manager) \
                  registers the PCI and platform bus types before any bus instance or driver \
                  refers to them, as the Panics section documents"
    )]
    pub(crate) fn find_bus_type(&self, id: BusTypeId) -> Arc<BusTypeObject> {
        self.bus_types
            .iter()
            .find(|bus_type| bus_type.id() == id)
            .cloned()
            .unwrap_or_else(|| {
                panic!(
                    "BusType {:?} was not registered before use; call register_bus_type() during \
                     init",
                    id
                )
            })
    }

    pub(crate) fn add_bus(&mut self, bus: Arc<BusInstance>) {
        self.buses.insert(bus.id(), bus);
    }

    pub(crate) fn add_driver(&mut self, driver: Arc<DriverObject>) {
        self.drivers.insert(driver.id(), driver);
    }

    pub(crate) fn add_device(&mut self, device: Arc<DeviceObject>) {
        self.devices.insert(device.id(), device);
    }

    pub(crate) fn remove_device(&mut self, id: DeviceId) -> Option<Arc<DeviceObject>> {
        self.devices.remove(&id)
    }

    /// Snapshot all discovery descriptors.
    pub(crate) fn device_descs_snapshot(&self) -> Vec<DeviceDesc> {
        self.descriptors
            .values()
            .map(|entry| entry.desc.clone())
            .collect()
    }

    /// Snapshot all registered bus metadata.
    pub(crate) fn buses_snapshot(&self) -> Vec<BusInfo> {
        self.buses.values().map(|bus| bus.info().clone()).collect()
    }

    /// Snapshot all registered driver metadata.
    pub(crate) fn drivers_snapshot(&self) -> Vec<DriverInfo> {
        self.drivers.values().map(|driver| driver.info()).collect()
    }

    /// Snapshot firmware match specs from drivers registered on one bus type.
    pub(crate) fn firmware_match_specs_for_bus_type(
        &self,
        bus_type: BusTypeId,
    ) -> Vec<FirmwareMatchSpec> {
        self.drivers
            .values()
            .filter(|driver| driver.bus_types().contains(&bus_type))
            .filter_map(|driver| driver.ops().matcher().firmware_spec().copied())
            .collect()
    }

    /// Snapshot all live device records from device objects.
    pub(crate) fn records_snapshot(&self) -> Vec<DeviceRecord> {
        self.devices
            .values()
            .map(|device| device.record_snapshot())
            .collect()
    }

    /// Register a subscriber for one lifecycle event kind.
    pub(crate) fn subscribe_kind(&mut self, kind: DeviceEventKind, callback: DeviceEventCallback) {
        self.subscribers.subscribe_kind(kind, callback);
    }

    /// Snapshot the lifecycle subscribers registered for `kind`. Callbacks
    /// must run outside locks.
    pub(crate) fn subscribers_for(&self, kind: DeviceEventKind) -> Vec<DeviceEventCallback> {
        self.subscribers.subscribers_for(kind)
    }
}

/// Initialize the global device index.
pub fn init_device_registry() {
    if !DEVICE_REGISTRY.is_inited() {
        DEVICE_REGISTRY.init_once(SpinNoPreempt::new(DeviceRegistry::new()));
    }
}

/// Acquire the global object index.
pub(crate) fn device_registry() -> SpinNoPreemptGuard<'static, DeviceRegistry> {
    init_device_registry();
    DEVICE_REGISTRY.lock()
}

/// Register a bus type object with a custom matcher in the global registry.
pub fn register_bus_type(bus_type: Arc<BusTypeObject>) {
    device_registry().register_bus_type(bus_type);
}

/// Find a device object by ID.
pub fn find_device(id: DeviceId) -> Option<Arc<DeviceObject>> {
    device_registry().find_device(id)
}

/// Find a bus instance by ID.
pub fn find_bus(id: BusId) -> Option<Arc<BusInstance>> {
    device_registry().find_bus(id)
}

/// Find a bus instance by its human-readable name.
pub fn find_bus_by_name(name: &str) -> Option<Arc<BusInstance>> {
    device_registry().find_bus_by_name(name)
}

/// Find a discovery descriptor by ID.
pub fn find_device_desc(id: DeviceDescId) -> Option<DeviceDesc> {
    device_registry().find_device_desc(id)
}

/// Find a registered driver object by name.
pub fn find_driver_by_name(name: &str) -> Option<Arc<DriverObject>> {
    device_registry().find_driver_by_name(name)
}

/// Find a registered driver object by ID.
pub fn find_driver(id: DriverId) -> Option<Arc<DriverObject>> {
    device_registry().find_driver(id)
}

/// Snapshot all discovery descriptors.
pub fn device_descs_snapshot() -> Vec<DeviceDesc> {
    device_registry().device_descs_snapshot()
}

/// Snapshot all live device records.
pub fn device_records_snapshot() -> Vec<DeviceRecord> {
    device_registry().records_snapshot()
}

/// Snapshot all registered bus metadata.
pub fn bus_infos_snapshot() -> Vec<BusInfo> {
    device_registry().buses_snapshot()
}

/// Snapshot all registered driver metadata.
pub fn driver_infos_snapshot() -> Vec<DriverInfo> {
    device_registry().drivers_snapshot()
}

/// Snapshot firmware match specs from drivers registered on one bus type.
pub fn firmware_match_specs_for_bus_type(bus_type: BusTypeId) -> Vec<FirmwareMatchSpec> {
    device_registry().firmware_match_specs_for_bus_type(bus_type)
}

#[cfg(unittest)]
pub fn reset_device_registry_for_tests() {
    init_device_registry();
    let mut index = DEVICE_REGISTRY.lock();
    index.descriptors.clear();
    index.devices.clear();
    index.buses.clear();
    index.bus_types.clear();
    index.drivers.clear();
    index.subscribers.clear();
    NEXT_BUS_ID.store(0, Ordering::Relaxed);
    NEXT_DRIVER_ID.store(0, Ordering::Relaxed);
    NEXT_DESC_ID.store(0, Ordering::Relaxed);
    NEXT_DEVICE_ID.store(0, Ordering::Relaxed);
}

/// Atomically reset the registry and set up a platform bus type with one bus
/// instance.  Returns `(Arc<BusTypeObject>, Arc<BusInstance>)`.
///
/// Holding the lock across reset + setup eliminates the SMP race window where
/// another CPU's `reset_model()` could swap the bus type object between a
/// caller's `device_desc_add` and its `register_driver_object`.
#[cfg(unittest)]
pub fn reset_and_setup_platform_bus_for_tests(
    bus_name: &'static str,
) -> (Arc<BusTypeObject>, Arc<BusInstance>) {
    use crate::{BusInfo, BusTypeObject, PlatformBusTypeMatcher};

    init_device_registry();
    let mut index = DEVICE_REGISTRY.lock();

    // Reset all state under the same lock.
    index.descriptors.clear();
    index.devices.clear();
    index.buses.clear();
    index.bus_types.clear();
    index.drivers.clear();
    index.subscribers.clear();
    NEXT_BUS_ID.store(0, Ordering::Relaxed);
    NEXT_DRIVER_ID.store(0, Ordering::Relaxed);
    NEXT_DESC_ID.store(0, Ordering::Relaxed);
    NEXT_DEVICE_ID.store(0, Ordering::Relaxed);

    // Set up bus type + bus instance while still holding the lock.
    let bus_type = Arc::new(BusTypeObject::new(Arc::new(PlatformBusTypeMatcher::new())));
    index.register_bus_type(bus_type.clone());

    let bus_id = index.alloc_bus_id();
    let bus = Arc::new(BusInstance::new(BusInfo {
        id: bus_id,
        bus_type_id: bus_type.id(),
        name: bus_name,
    }));
    index.add_bus(bus.clone());

    (bus_type, bus)
}
