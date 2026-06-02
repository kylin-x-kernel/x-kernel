// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Driver-core lifecycle orchestration over discovery descriptors and live objects.

pub(crate) mod dispatch;
pub mod event;
pub(crate) mod subscribers;

use alloc::{sync::Arc, vec::Vec};

use driver_base::DriverError;

use self::{
    dispatch::{
        activate_device, bind_device_to_driver, dispatch_device_event, mark_device_matched,
    },
    event::DeviceEvent,
};
use crate::{
    BusId, BusInstance, BusTypeId, BusTypeObject, DeviceDesc, DeviceDescId, DeviceId,
    DeviceIdentity, DeviceLocation, DeviceObject, DiscoveryOrigin, DriverObject, MatchResult,
    ResourceSet, device_registry, init_device_registry, registry::DeviceDescState,
};

/// Description of a boot/runtime handoff object that is already active before
/// the generic driver probe pipeline runs.
pub struct ActiveDeviceAdoption {
    pub bus_id: BusId,
    /// Optional parent device under which this object should be attached.
    pub parent: Option<DeviceId>,
    pub location: DeviceLocation,
    pub origin: DiscoveryOrigin,
    pub identity: DeviceIdentity,
    pub transport: Option<crate::TransportInfo>,
    pub resources: ResourceSet,
    pub driver: Arc<DriverObject>,
}

type UnpublishedDescDevice = (Arc<DeviceObject>, Arc<BusTypeObject>);

/// Result of one driver-core probe attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The probe activated the device in this call.
    Activated,
    /// The device was already active or removed and was not reprobed.
    Skipped,
    /// No registered driver matched this device.
    Requeue,
    /// Matching drivers existed but none succeeded; the descriptor remains unclaimed.
    Unclaimed,
}

impl ProbeOutcome {
    /// Whether this attempt activated a device.
    pub const fn activated(self) -> bool {
        matches!(self, Self::Activated)
    }
}

fn bus_type_for_desc(desc: &DeviceDesc) -> Option<Arc<BusTypeObject>> {
    let index = device_registry();
    let bus = index.find_bus(desc.bus_id())?;
    Some(index.find_bus_type(bus.bus_type()))
}

fn pending_descriptors_for_bus_types(bus_types: &[BusTypeId]) -> Vec<DeviceDesc> {
    // Lock-order discipline: snapshot the BusTypeObject Arcs under the
    // registry guard, drop it, then read pending descriptor IDs from each
    // bus-type object. Holding the registry guard while taking per-object
    // locks would invert the documented order (registry -> per-object
    // snapshot only).
    let bus_type_objs: Vec<Arc<BusTypeObject>> = {
        let index = device_registry();
        bus_types
            .iter()
            .copied()
            .map(|bus_type| index.find_bus_type(bus_type))
            .collect()
    };
    let desc_ids = bus_type_objs
        .iter()
        .flat_map(|bus_type| bus_type.pending_descriptors_snapshot())
        .collect::<Vec<_>>();

    let index = device_registry();
    desc_ids
        .into_iter()
        .filter_map(|desc_id| index.find_pending_device_desc(desc_id))
        .collect()
}

/// Register a bus instance and attach it to its bus type.
pub fn register_bus_instance(bus_type_id: BusTypeId, name: &'static str) -> Arc<BusInstance> {
    init_device_registry();

    let (bus, bus_type) = {
        let mut index = device_registry();
        let id = index.alloc_bus_id();
        let info = crate::BusInfo {
            id,
            bus_type_id,
            name,
        };
        let bus = Arc::new(BusInstance::new(info));
        let bus_type = index.find_bus_type(bus_type_id);
        index.add_bus(bus.clone());
        (bus, bus_type)
    };

    bus_type.attach_bus(bus.id());
    debug_assert!(bus_type.has_bus(bus.id()));
    bus
}

/// Register a driver object and attach it to every supported bus type.
pub fn register_driver_object(ops: Arc<dyn crate::DeviceDriver>) -> Arc<DriverObject> {
    init_device_registry();

    let (driver, bus_types) = {
        let mut index = device_registry();
        let id = index.alloc_driver_id();
        let driver = Arc::new(DriverObject::new(id, ops));
        let bus_types = driver
            .bus_types()
            .iter()
            .copied()
            .map(|bus_type| index.find_bus_type(bus_type))
            .collect::<Vec<_>>();
        index.add_driver(driver.clone());
        (driver, bus_types)
    };

    for bus_type in bus_types {
        bus_type.attach_driver(driver.clone());
    }

    let mut activated_from_desc = 0usize;
    for desc in pending_descriptors_for_bus_types(driver.bus_types()) {
        match probe_device_desc_with_drivers(desc, alloc::vec![driver.clone()]) {
            Ok(outcome) if outcome.activated() => activated_from_desc += 1,
            Ok(_) => {}
            Err(error) => log::warn!(
                "driver {} descriptor reprobe failed: {:?}",
                driver.name(),
                error
            ),
        }
    }
    if activated_from_desc > 0 {
        log::debug!(
            "driver {} activated {} existing descriptor(s)",
            driver.name(),
            activated_from_desc,
        );
    }

    driver
}

/// Register a discovery-stage device descriptor without creating a runtime object.
pub fn device_desc_add(
    bus_id: BusId,
    location: DeviceLocation,
    origin: DiscoveryOrigin,
    identity: DeviceIdentity,
    transport: Option<crate::TransportInfo>,
    resources: ResourceSet,
) -> Result<DeviceDesc, DriverError> {
    device_desc_add_with_parent(
        bus_id, None, location, origin, identity, transport, resources,
    )
}

/// Same as [`device_desc_add`], but records a parent device the resulting
/// runtime object will be attached under.
#[allow(clippy::too_many_arguments)]
pub fn device_desc_add_with_parent(
    bus_id: BusId,
    parent: Option<DeviceId>,
    location: DeviceLocation,
    origin: DiscoveryOrigin,
    identity: DeviceIdentity,
    transport: Option<crate::TransportInfo>,
    resources: ResourceSet,
) -> Result<DeviceDesc, DriverError> {
    init_device_registry();

    let (desc, bus_type) = {
        let mut index = device_registry();
        let bus = index.find_bus(bus_id).ok_or(DriverError::InvalidInput)?;
        let bus_type = index.find_bus_type(bus.bus_type());
        let id = index.alloc_device_desc_id();
        let desc = DeviceDesc::new_with_parent(
            id, bus_id, parent, location, origin, identity, transport, resources,
        );
        index.add_device_desc(desc.clone());
        (desc, bus_type)
    };

    bus_type.enqueue_pending_descriptor(desc.id());
    Ok(desc)
}

fn desc_probe_outcome(id: DeviceDescId) -> Option<Result<ProbeOutcome, DriverError>> {
    let (state, device) = {
        let index = device_registry();
        let state = index.device_desc_state(id)?;
        let device = match state {
            DeviceDescState::Bound(_) => index.find_device_for_desc(id),
            _ => None,
        };
        (state, device)
    };

    match state {
        DeviceDescState::Pending => None,
        DeviceDescState::Probing => Some(Ok(ProbeOutcome::Requeue)),
        DeviceDescState::Bound(_) => Some(match device {
            Some(device)
                if matches!(
                    device.state(),
                    crate::DeviceState::Active
                        | crate::DeviceState::Removing
                        | crate::DeviceState::Removed
                ) =>
            {
                Ok(ProbeOutcome::Skipped)
            }
            Some(_) => Ok(ProbeOutcome::Requeue),
            None => Err(DriverError::BadState),
        }),
    }
}

fn new_unpublished_device_from_desc(
    desc: &DeviceDesc,
) -> Result<UnpublishedDescDevice, DriverError> {
    // Clone the descriptor's resources before taking the registry lock so the
    // (potentially heap-allocating) snapshot does not run under the lock.
    let resources = desc.resources_snapshot();
    let (id, bus_type) = {
        let mut index = device_registry();
        let bus = index
            .find_bus(desc.bus_id())
            .ok_or(DriverError::InvalidInput)?;
        let bus_type = index.find_bus_type(bus.bus_type());
        let id = index.alloc_device_id();
        (id, bus_type)
    };
    // Allocate the device object outside the registry lock.
    let device = Arc::new(DeviceObject::new(
        id,
        desc.bus_id(),
        desc.location(),
        desc.origin(),
        desc.identity(),
        desc.transport(),
        resources,
    ));
    Ok((device, bus_type))
}

fn publish_desc_device(
    desc_id: DeviceDescId,
    device: Arc<DeviceObject>,
    bus_type: Arc<BusTypeObject>,
    driver: Arc<DriverObject>,
) {
    // Lock-order discipline: snapshot the bus instance Arc under the registry
    // guard, drop the guard, then mutate the bus instance object outside.
    let bus_instance = {
        let mut index = device_registry();
        index.add_device(device.clone());
        index.bind_device_desc(desc_id, device.id());
        index.find_bus(device.bus_id())
    };
    if let Some(bus) = bus_instance {
        bus.add_device(device.clone());
        bus.add_driver(driver);
    }
    bus_type.remove_pending_descriptor(desc_id);
}

fn requeue_desc(desc_id: DeviceDescId) {
    device_registry().requeue_device_desc(desc_id);
}

fn drivers_for_desc(desc: &DeviceDesc) -> Vec<Arc<DriverObject>> {
    let Some(bus_type) = bus_type_for_desc(desc) else {
        return Vec::new();
    };
    bus_type.drivers_snapshot()
}

fn probe_device_desc_with_drivers(
    desc: DeviceDesc,
    candidates: Vec<Arc<DriverObject>>,
) -> Result<ProbeOutcome, DriverError> {
    if let Some(outcome) = desc_probe_outcome(desc.id()) {
        return outcome;
    }

    let Some(bus_type) = bus_type_for_desc(&desc) else {
        return Err(DriverError::InvalidInput);
    };

    if device_registry()
        .mark_device_desc_probing(desc.id())
        .is_none()
    {
        return Ok(ProbeOutcome::Requeue);
    }

    // Best-match-only: pick the single highest-priority matching driver and
    // bind it against one unpublished DeviceObject. Allocating a fresh
    // DeviceId per candidate would conflate "one device" with "one probe
    // attempt" and leak orphan IDs into subscribers; instead, on probe
    // failure we requeue the descriptor and let the next reprobe pass try a
    // different driver. Drivers already attempted (and failed) for this
    // descriptor are excluded so we advance to the next-best candidate
    // instead of reselecting the same failing driver forever.
    let attempted = device_registry().device_desc_attempted(desc.id());
    let best = candidates
        .into_iter()
        .filter(|driver| !attempted.contains(&driver.id()))
        .filter_map(|driver| {
            let ops = driver.ops();
            match bus_type.match_desc(ops.as_ref(), &desc) {
                MatchResult::Match { priority } => Some((driver, priority)),
                MatchResult::NoMatch => None,
            }
        })
        .max_by_key(|candidate| candidate.1);

    let Some((driver, _priority)) = best else {
        requeue_desc(desc.id());
        return Ok(ProbeOutcome::Requeue);
    };

    let (device, publish_bus_type) = match new_unpublished_device_from_desc(&desc) {
        Ok(unpublished) => unpublished,
        Err(error) => {
            requeue_desc(desc.id());
            return Err(error);
        }
    };

    // Drive the normal probe path through the same dispatch helpers used by
    // adopt_active_device, so subscribers always see the canonical sequence
    // Matched -> Bound -> Published -> Activated regardless of how the
    // object reached the driver core.
    mark_device_matched(device.as_ref());
    bind_device_to_driver(
        device.as_ref(),
        driver.id(),
        driver.name(),
        driver.device_kind(),
    );

    let ops = driver.ops();
    // Account this probe attempt on both the driver and its bus instance so
    // repeated failures are observable via topology/procfs (review item O6).
    let bus_instance = device_registry().find_bus(desc.bus_id());
    driver.record_probe_attempt();
    if let Some(bus) = &bus_instance {
        bus.record_probe_attempt();
    }
    match ops.probe_device(device.clone()) {
        Ok(()) => {
            publish_desc_device(desc.id(), device.clone(), publish_bus_type, driver.clone());
            if let Some(parent_id) = desc.parent()
                && let Err(err) = attach_device_parent(parent_id, device.id())
            {
                log::warn!(
                    "failed to attach device {:?} under parent {:?}: {:?}",
                    device.id(),
                    parent_id,
                    err,
                );
            }
            driver.attach_device(device.id());
            dispatch_device_event(DeviceEvent::Published { id: device.id() });
            activate_device(device.as_ref(), driver.device_kind());
            Ok(ProbeOutcome::Activated)
        }
        Err(error) => {
            log::info!(
                "driver {} probe failed for descriptor {:?}: {:?}",
                driver.name(),
                desc.id(),
                error,
            );
            driver.record_probe_failure();
            if let Some(bus) = &bus_instance {
                bus.record_probe_failure();
            }
            // Roll back driver binding metadata so the unpublished object is
            // dropped without leaving Bound state visible anywhere. Run any
            // devres cleanups the driver registered before it failed so a
            // partially-initialized probe cannot leak resources.
            device.run_cleanups();
            device.detach_driver();
            // Record the failed driver so reprobe passes skip it and move on
            // to the next-best candidate.
            device_registry().mark_device_desc_attempted(desc.id(), driver.id());
            requeue_desc(desc.id());
            log::debug!(
                "descriptor {:?} probe exhausted candidates: {:?}",
                desc.id(),
                error,
            );
            // Preserve the original error context for callers that want to
            // surface the probe failure root cause.
            let _ = error;
            Ok(ProbeOutcome::Unclaimed)
        }
    }
}

/// Probe one registered descriptor through the descriptor-first driver path.
pub fn probe_device_desc(id: DeviceDescId) -> Result<ProbeOutcome, DriverError> {
    let desc = device_registry()
        .find_device_desc(id)
        .ok_or(DriverError::InvalidInput)?;
    probe_device_desc_with_drivers(desc.clone(), drivers_for_desc(&desc))
}

/// Adopt a device that boot code already initialized, skipping descriptor match/probe.
///
/// This path is for explicit boot/runtime handoff contracts. It creates a
/// device object, registers it in the driver-core registry, and records the
/// driver binding metadata. It drives the same canonical lifecycle event
/// sequence as the probe path (`Matched` -> `Bound` -> `Published` ->
/// `Activated`) so that subscribers see adopted devices (e.g. PCI host
/// bridges, PCI-PCI bridges) the same way they see probed ones; when adoption
/// runs before any subscriber is registered the dispatches are simply no-ops.
/// The caller is responsible for completing subsystem-level registration
/// (e.g. `publish_char`).
pub fn adopt_active_device(
    adoption: ActiveDeviceAdoption,
) -> Result<Arc<DeviceObject>, DriverError> {
    init_device_registry();

    let driver = adoption.driver;
    // Validate the target bus and mint the IDs under the registry lock, then
    // drop it before constructing the (heap-allocating) descriptor and device
    // objects so allocation does not run under the lock.
    let (bus_instance, desc_id, id) = {
        let mut index = device_registry();
        let bus_instance = index.find_bus(adoption.bus_id);
        let bus = bus_instance.as_ref().ok_or(DriverError::InvalidInput)?;
        if !driver.bus_types().contains(&bus.bus_type()) {
            return Err(DriverError::InvalidInput);
        }
        let desc_id = index.alloc_device_desc_id();
        let id = index.alloc_device_id();
        (bus_instance, desc_id, id)
    };

    let desc = DeviceDesc::new_with_parent(
        desc_id,
        adoption.bus_id,
        adoption.parent,
        adoption.location,
        adoption.origin,
        adoption.identity,
        adoption.transport,
        adoption.resources.clone(),
    );
    let device = Arc::new(DeviceObject::new(
        id,
        adoption.bus_id,
        adoption.location,
        adoption.origin,
        adoption.identity,
        adoption.transport,
        adoption.resources,
    ));

    {
        let mut index = device_registry();
        index.add_device_desc(desc);
        index.add_device(device.clone());
        index.bind_device_desc(desc_id, device.id());
    }

    // Bus instance must be mutated outside the registry guard per the
    // lock-order discipline.
    if let Some(bus) = bus_instance {
        bus.add_device(device.clone());
        bus.add_driver(driver.clone());
    }

    // Drive the canonical lifecycle event sequence through the same dispatch
    // helpers used by the probe path so subscribers observe an adopted device
    // identically: Matched -> Bound -> Published -> Activated.
    mark_device_matched(device.as_ref());
    bind_device_to_driver(
        device.as_ref(),
        driver.id(),
        driver.name(),
        driver.device_kind(),
    );
    driver.attach_device(device.id());

    if let Some(parent_id) = adoption.parent
        && let Err(err) = attach_device_parent(parent_id, device.id())
    {
        log::warn!(
            "failed to attach adopted device {:?} under parent {:?}: {:?}",
            device.id(),
            parent_id,
            err,
        );
    }

    dispatch_device_event(DeviceEvent::Published { id: device.id() });
    activate_device(device.as_ref(), driver.device_kind());

    Ok(device)
}

/// Attach a child device under a parent device object.
pub fn attach_device_parent(parent_id: DeviceId, child_id: DeviceId) -> Result<(), DriverError> {
    let (parent, child) = {
        let index = device_registry();
        let parent = index
            .find_device(parent_id)
            .ok_or(DriverError::InvalidInput)?;
        let child = index
            .find_device(child_id)
            .ok_or(DriverError::InvalidInput)?;
        (parent, child)
    };

    parent.attach_child(child_id);
    child.set_parent(Some(parent_id));
    Ok(())
}

/// Detach a child device from its current parent.
pub fn detach_device_parent(child_id: DeviceId) -> Result<(), DriverError> {
    let (parent, child) = {
        let index = device_registry();
        let child = index
            .find_device(child_id)
            .ok_or(DriverError::InvalidInput)?;
        let parent = child
            .parent()
            .and_then(|parent_id| index.find_device(parent_id));
        (parent, child)
    };

    if let Some(parent) = parent {
        parent.detach_child(child_id);
    }
    child.set_parent(None);
    Ok(())
}

/// Mark a device removed in the object index and notify driver-core observers.
pub fn remove_device_from_index(id: DeviceId) {
    // Lock-order discipline: take Arc snapshots under the registry guard,
    // drop the guard, then mutate per-object state. This avoids the
    // registry -> per-object nesting that was previously inverting the
    // documented lock order.
    let (device, bus, bus_type, driver, parent, pending_descs, children) = {
        let mut index = device_registry();
        let Some(device) = index.remove_device(id) else {
            return;
        };
        let bus = index.find_bus(device.bus_id());
        let bus_type = bus.as_ref().map(|bus| index.find_bus_type(bus.bus_type()));
        let pending_descs = index.requeue_device_descs_for_device(id);
        // Any descriptor still pointing at this device as its parent is now
        // stale; clear the linkage so a later reprobe does not try to attach
        // under a removed parent.
        index.clear_descriptor_parents_for(id);
        let driver = device
            .driver_id()
            .and_then(|driver_id| index.find_driver(driver_id));
        let parent = device
            .parent()
            .and_then(|parent_id| index.find_device(parent_id));
        // Live children still reference this device as their parent; snapshot
        // them so we can drop the now-dangling parent pointer below.
        let children = device
            .children_snapshot()
            .into_iter()
            .filter_map(|child_id| index.find_device(child_id))
            .collect::<alloc::vec::Vec<_>>();
        (
            device,
            bus,
            bus_type,
            driver,
            parent,
            pending_descs,
            children,
        )
    };

    if let Some(bus) = &bus {
        bus.remove_device(id);
    }
    // Drop bound-driver metadata before announcing removal so a Removed
    // subscriber that inspects the object never observes a Bound/Active
    // state with no driver pointer.
    device.detach_driver();
    device.mark_removed();
    if let Some(bus_type) = bus_type {
        for desc_id in pending_descs {
            bus_type.enqueue_pending_descriptor(desc_id);
        }
    }
    if let Some(driver) = driver {
        driver.detach_device(id);
        if let Some(bus) = bus {
            bus.remove_driver(driver.id());
        }
    }
    if let Some(parent) = parent {
        parent.detach_child(id);
    }
    // Drop the dangling parent pointer on any live children so the topology
    // never exposes a child referencing a removed parent.
    for child in children {
        child.set_parent(None);
    }
    dispatch_device_event(DeviceEvent::Removed { id });
}

/// Run the driver-core remove path for a live device.
pub fn remove_device_managed(id: DeviceId) -> Result<(), DriverError> {
    let (device, driver, bus_type) = {
        let index = device_registry();
        let device = index.find_device(id).ok_or(DriverError::InvalidInput)?;
        let driver = device
            .driver_id()
            .and_then(|driver_id| index.find_driver(driver_id));
        let bus_type = index
            .find_bus(device.bus_id())
            .map(|bus| index.find_bus_type(bus.bus_type()));
        (device, driver, bus_type)
    };

    // `begin_removing` is the single commit point: it atomically claims the
    // device for removal (or fails with `ResourceBusy` if another remove is
    // already in flight). Crucially it must run *before* any teardown so we
    // never start tearing down a device twice.
    if !device.begin_removing() {
        return Err(DriverError::ResourceBusy);
    }

    // Past the commit point removal is irreversible. `driver.remove` and
    // `bus_type.remove` are best-effort cleanup hooks; once `driver.remove`
    // has run the device's private state (MMIO mappings, IRQ handlers, queues)
    // may already be gone, so we must NOT roll the lifecycle back to a usable
    // state on failure — doing so would expose a torn-down device as `Active`
    // and invite use-after-free. Failures are logged and removal proceeds to
    // `Removed`. This mirrors the "remove never fails" contract of Linux's
    // driver core.
    if let Some(driver) = driver
        && let Err(error) = driver.ops().remove(device.clone())
    {
        log::error!(
            "driver {} remove() failed for device {:?}: {:?}; proceeding with removal",
            driver.name(),
            id,
            error,
        );
    }

    if let Some(bus_type) = bus_type
        && let Err(error) = bus_type.remove(device.clone())
    {
        log::error!(
            "bus-type remove() failed for device {:?}: {:?}; proceeding with removal",
            id,
            error,
        );
    }

    // Release any driver-registered resources that outlived the driver's own
    // remove() hook. Runs after both teardown hooks so the driver gets first
    // chance to release things itself.
    device.run_cleanups();

    remove_device_from_index(id);
    Ok(())
}

#[cfg(unittest)]
mod tests {
    use alloc::sync::Arc;

    use driver_base::{DeviceKind, DriverResult};
    use unittest::{assert_eq, def_test};

    use super::*;
    use crate::{DeviceDriver, DeviceState, PlatformIdentity};

    struct MatchingDriver;

    impl DeviceDriver for MatchingDriver {
        fn name(&self) -> &'static str {
            "matching-test"
        }

        fn device_kind(&self) -> DeviceKind {
            DeviceKind::Char
        }

        fn bus_types(&self) -> &'static [BusTypeId] {
            &[BusTypeId::PLATFORM]
        }

        fn matcher(&self) -> &dyn crate::DeviceMatcher {
            static M: crate::CompatibleAliasMatcher = crate::CompatibleAliasMatcher("matched");
            &M
        }

        fn probe_device(&self, _device: Arc<DeviceObject>) -> DriverResult<()> {
            Ok(())
        }
    }

    fn reset_model() -> Arc<BusInstance> {
        let (_bus_type, bus) = crate::reset_and_setup_platform_bus_for_tests("descriptor-test-bus");
        bus
    }

    #[def_test(serial)]
    fn test_unmatched_descriptor_probe_does_not_create_runtime_object() {
        let bus = reset_model();
        let desc = device_desc_add(
            bus.id(),
            DeviceLocation::PlatformStatic { id: 1 },
            DiscoveryOrigin::PlatformStatic,
            DeviceIdentity::Platform(PlatformIdentity {
                alias: Some("unmatched"),
                firmware_id: None,
            }),
            None,
            ResourceSet::new(),
        )
        .unwrap();

        assert_eq!(crate::device_records_snapshot().len(), 0);
        assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Requeue);
        assert_eq!(crate::device_records_snapshot().len(), 0);
    }

    #[def_test(serial)]
    fn test_driver_registration_scans_existing_descriptors() {
        let bus = reset_model();
        device_desc_add(
            bus.id(),
            DeviceLocation::PlatformStatic { id: 2 },
            DiscoveryOrigin::PlatformStatic,
            DeviceIdentity::Platform(PlatformIdentity {
                alias: Some("matched"),
                firmware_id: None,
            }),
            None,
            ResourceSet::new(),
        )
        .unwrap();

        assert_eq!(crate::device_records_snapshot().len(), 0);
        register_driver_object(Arc::new(MatchingDriver));

        let records = crate::device_records_snapshot();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].state, DeviceState::Active);
        assert_eq!(records[0].driver_name, Some("matching-test"));
    }

    #[def_test(serial)]
    fn test_adopt_active_device_registers_bound_object() {
        let bus = reset_model();
        let driver = register_driver_object(Arc::new(MatchingDriver));

        let device = adopt_active_device(ActiveDeviceAdoption {
            bus_id: bus.id(),
            parent: None,
            location: DeviceLocation::PlatformStatic { id: 3 },
            origin: DiscoveryOrigin::PlatformStatic,
            identity: DeviceIdentity::Platform(PlatformIdentity {
                alias: Some("adopted"),
                firmware_id: None,
            }),
            transport: None,
            resources: ResourceSet::new(),
            driver,
        })
        .unwrap();

        let records = crate::device_records_snapshot();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, device.id());
        assert_eq!(records[0].state, DeviceState::Active);
        assert_eq!(records[0].driver_name, Some("matching-test"));

        let descriptors = crate::device_descs_snapshot();
        assert_eq!(descriptors.len(), 1);
        assert_eq!(
            descriptors[0].identity(),
            DeviceIdentity::Platform(PlatformIdentity {
                alias: Some("adopted"),
                firmware_id: None
            })
        );
        let desc_device = crate::device_registry()
            .find_device_for_desc(descriptors[0].id())
            .unwrap();
        assert_eq!(desc_device.id(), device.id());
    }
}
