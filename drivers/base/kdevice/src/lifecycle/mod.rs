// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Driver-core lifecycle orchestration over discovery descriptors and stable
//! device objects.

pub(crate) mod dispatch;
pub(crate) mod event;
pub(crate) mod subscribers;

use alloc::{sync::Arc, vec::Vec};

use driver_base::DriverError;
use kmutex::Mutex;

use self::{
    dispatch::{
        activate_device, announce_device_bound, announce_device_matched, dispatch_device_event,
        prepare_device_binding,
    },
    event::DeviceEvent,
};
use crate::{
    BindingRef, BusId, BusInstance, BusTypeId, BusTypeObject, DeviceDesc, DeviceDescId,
    DeviceDescSpec, DeviceId, DeviceIdentity, DeviceLocation, DeviceObject, DeviceState,
    DiscoveryOrigin, DriverObject, MatchResult, ResourceSet, device_registry, init_device_registry,
    registry::DeviceDescState,
    topology::relations::{
        attach_device_parent_locked, clear_registration_topology, detach_device_parent_locked,
    },
};

/// Description of a boot/runtime handoff object that is already active before
/// the generic driver probe pipeline runs.
pub struct ActiveDeviceAdoption {
    /// Bus instance the device is attached to.
    pub bus_id: BusId,
    /// Optional parent device under which this object should be attached.
    pub parent: Option<DeviceId>,
    /// Where on the bus hierarchy the device lives.
    pub location: DeviceLocation,
    /// Which firmware source described the device.
    pub origin: DiscoveryOrigin,
    /// Bus-specific identity used for matching bookkeeping.
    pub identity: DeviceIdentity,
    /// Upper-layer transport descriptor, if any.
    pub transport: Option<crate::TransportInfo>,
    /// Resources already acquired for the running device.
    pub resources: ResourceSet,
    /// Resolved DMA view, when boot code already composed one.
    pub dma: Option<driver_os_api::DmaInfo>,
    /// Driver that owns the running device and will be recorded as bound.
    pub driver: Arc<DriverObject>,
}

/// Result of one driver-core probe attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The probe activated the device in this call.
    Activated,
    /// The device was already active or removed and was not reprobed.
    Skipped,
    /// The current execution context already holds this descriptor's binding
    /// lock, so the recursive attempt was rejected without waiting.
    ///
    /// This is not a completed attempt and is never returned for cross-thread
    /// contention: another task waits on the lock and rechecks the state.
    Busy,
    /// No driver in this round's candidate snapshot matched; no probe ran.
    Unmatched,
    /// At least one matching candidate was probed and all failed normally.
    /// The descriptor is back to `Pending` and may be retried.
    Unclaimed,
}

impl ProbeOutcome {
    /// Whether this attempt activated a device.
    pub const fn activated(self) -> bool {
        matches!(self, Self::Activated)
    }
}

/// Deterministic interleaving points used by unit tests.
///
/// These seams exist only in `cfg(unittest)` builds. A callback may block (for
/// example on a test `ksync` semaphore) because every pause point is outside
/// registry/bus spinlocks. The probe, adoption, and remove points are inside
/// the relevant binding critical section; the registration-scan point fires
/// just before that scan takes the lock.
#[cfg(unittest)]
#[path = "binding_hooks.rs"]
pub mod test_hooks;

fn bus_type_for_desc(desc: &DeviceDesc) -> Option<Arc<BusTypeObject>> {
    let index = device_registry();
    let bus = index.find_bus(desc.bus_id())?;
    Some(index.find_bus_type(bus.bus_type()))
}

/// Candidate set for one binding round.
enum ProbeCandidates {
    /// Device-first path: the descriptor's bus type in registration order.
    BusType,
    /// Driver-first path: only the newly registered driver.
    Only { driver: Arc<DriverObject> },
}

/// Re-read a descriptor after waiting for its binding lock.
///
/// The caller must hold the binding lock. `Some` carries the `Pending`
/// descriptor the caller may claim and probe; `None` means the descriptor was
/// unregistered while the caller waited or is bound to a stable object that is
/// already terminal. `Probing` here means the state invariant was broken
/// (only the lock holder may set it), and a `Bound` descriptor whose object is
/// neither terminal nor registered means publication did not finish before the
/// lock was released.
fn inspect_locked_desc(id: DeviceDescId) -> Result<Option<DeviceDesc>, DriverError> {
    let (state, desc, device) = {
        let index = device_registry();
        let Some(state) = index.device_desc_state(id) else {
            // The descriptor's registration was removed by another task while
            // this caller waited for its binding lock. That is a legal race,
            // not a broken invariant.
            return Ok(None);
        };
        let desc = match state {
            DeviceDescState::Pending => index.find_device_desc(id),
            _ => None,
        };
        let device = match state {
            DeviceDescState::Bound => index.find_device_for_desc(id),
            _ => None,
        };
        (state, desc, device)
    };

    match state {
        DeviceDescState::Pending => desc.map(Some).ok_or(DriverError::InvalidInput),
        DeviceDescState::Probing => Err(DriverError::BadState),
        DeviceDescState::Bound => match device {
            Some(device)
                if matches!(
                    device.state(),
                    DeviceState::Active | DeviceState::Removing | DeviceState::Removed
                ) =>
            {
                Ok(None)
            }
            _ => Err(DriverError::BadState),
        },
    }
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
///
/// After the driver is attached, every `Pending` or `Probing` descriptor whose
/// bus type is in `driver.bus_types()` is given one binding attempt against this
/// driver alone. Each attempt takes that descriptor's binding lock, rechecks the
/// state after any wait, and skips the descriptor if another task bound it
/// first. A descriptor whose lock is already held by the current task (for
/// example a probe registering a helper driver) is also skipped, because
/// waiting on it would self-deadlock. Failures are logged and leave the
/// descriptor `Pending`; registration still returns the new driver.
///
/// A probe must not call this function for drivers that should bind to the
/// descriptor being probed; that recursion is rejected by the binding lock.
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

    let awaiting = {
        let index = device_registry();
        index.descriptors_awaiting_binding(driver.bus_types())
    };

    let mut activated_from_desc = 0usize;
    for desc_ref in awaiting {
        // A driver registered from inside a probe already holds that
        // descriptor's lock. Reject only this local recursion; a descriptor
        // held by another task is not `Busy`, it is waited for below.
        if desc_ref.binding.is_owned_by_current() {
            log::warn!(
                "driver {} registered while descriptor {:?} is being bound by the current \
                 context; skipping that descriptor",
                driver.name(),
                desc_ref.id,
            );
            continue;
        }

        #[cfg(unittest)]
        test_hooks::pause(test_hooks::BindingPause::RegisterScanBeforeLock {
            desc_id: desc_ref.id,
        });

        let _guard = desc_ref.binding.lock();
        let desc = match inspect_locked_desc(desc_ref.id) {
            Ok(Some(desc)) => desc,
            Ok(None) => continue,
            Err(error) => {
                log::warn!(
                    "driver {} descriptor {:?} recheck after wait failed: {:?}",
                    driver.name(),
                    desc_ref.id,
                    error,
                );
                continue;
            }
        };

        if device_registry()
            .mark_device_desc_probing(desc_ref.id)
            .is_none()
        {
            log::warn!(
                "driver {} descriptor {:?} could not be claimed for probe",
                driver.name(),
                desc_ref.id,
            );
            continue;
        }

        match probe_desc_round(
            desc,
            ProbeCandidates::Only {
                driver: driver.clone(),
            },
        ) {
            Ok(outcome) if outcome.activated() => activated_from_desc += 1,
            Ok(_) => {}
            Err(error) => log::warn!(
                "driver {} descriptor {:?} probe failed: {:?}",
                driver.name(),
                desc_ref.id,
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

/// Register a discovery-stage device and create its stable object.
///
/// The specification is consumed as one finalized discovery result. The device
/// core assigns the descriptor id and a stable [`DeviceId`], stores its own
/// immutable descriptor copy in `Pending` state, and publishes the stable
/// `DeviceObject` in `Discovered` state in the same registry critical section.
/// From registration onward that object identity survives probe failures,
/// unbind and rebind; only unregistering the descriptor ends it.
///
/// The new descriptor's binding lock is acquired with `try_lock` before the
/// object is published. This function may run under the enumeration
/// `SpinNoPreempt` lock, so it never blocks and never sleeps. The guard covers
/// the initial parent attach/recheck, so no probe can observe a half-linked
/// topology.
pub fn device_desc_add(spec: DeviceDescSpec) -> Result<DeviceDesc, DriverError> {
    init_device_registry();

    // Validate the bus and allocate both ids under the registry lock; construct
    // the (heap-allocating) descriptor, object and lock outside it.
    let (desc_id, device_id) = {
        let mut index = device_registry();
        if index.find_bus(spec.bus_id).is_none() {
            return Err(DriverError::InvalidInput);
        }
        (index.alloc_device_desc_id(), index.alloc_device_id())
    };

    let binding = Arc::new(Mutex::new(()));
    let desc = DeviceDesc::from_spec(desc_id, device_id, spec);
    let resources = desc.resources_snapshot();
    let device = Arc::new(
        DeviceObject::new_registered(
            device_id,
            desc.bus_id(),
            desc.location(),
            desc.origin(),
            desc.identity(),
            desc.transport(),
            resources,
            binding.clone(),
        )
        .with_dma(desc.dma())
        .with_intx_route(desc.intx_route())
        .with_firmware_node(desc.firmware_node()),
    );

    // No published handle exists for this lock yet, so `try_lock` cannot
    // contend with a probe/remove. The guard is deliberately held through
    // publication and the initial topology link/recheck.
    let _binding_guard = binding.try_lock().ok_or(DriverError::BadState)?;

    #[cfg(unittest)]
    test_hooks::pause(test_hooks::BindingPause::RegistrationHoldingLock { desc_id });

    {
        let mut index = device_registry();
        if index.find_bus(desc.bus_id()).is_none() {
            return Err(DriverError::InvalidInput);
        }
        if !index.add_device_desc_with_binding(desc.clone(), binding.clone()) {
            return Err(DriverError::BadState);
        }
        index.add_device(device.clone());
    }

    // "Link first, recheck membership" (§7.1): a concurrent unregister either
    // sees this link while snapshotting its children, or is detected by the
    // membership recheck and the link is conditionally undone.
    if let Some(parent_id) = desc.parent() {
        // The registry guard must be released before the topology helper takes
        // the registry again for its own snapshot.
        let parent_registered = device_registry().find_device(parent_id).is_some();
        if parent_registered && let Err(error) = attach_device_parent_locked(parent_id, device_id) {
            log::warn!(
                "failed to attach device {:?} under parent {:?} at registration: {:?}",
                device_id,
                parent_id,
                error,
            );
        }
    }

    Ok(desc)
}

fn publish_desc_device(
    desc_id: DeviceDescId,
    device: Arc<DeviceObject>,
    driver: Arc<DriverObject>,
) {
    // Lock-order discipline: snapshot the bus instance Arc under the registry
    // guard, drop the guard, then mutate the bus instance object outside.
    let bus_instance = {
        let mut index = device_registry();
        index.bind_device_desc(desc_id);
        index.find_bus(device.bus_id())
    };
    if let Some(bus) = bus_instance {
        bus.attach_bound_device(device, driver);
    }
}

fn requeue_desc(desc_id: DeviceDescId) {
    device_registry().requeue_device_desc(desc_id);
}

/// Run one binding round with the descriptor's binding lock already held.
///
/// The stable object already exists. Each matching driver gets a brand-new
/// binding runtime installed into the object's empty slot; a failed candidate
/// terminates that runtime, runs its cleanup batch and emits
/// `BindingRetired`, then clears the slot without discarding the object. The
/// first success stops the round. Ordinary probe failures are reported as
/// `Unclaimed` (logged and counted), never as a core error; core setup
/// failures requeue the descriptor and return `Err`.
fn probe_desc_round(
    desc: DeviceDesc,
    candidates: ProbeCandidates,
) -> Result<ProbeOutcome, DriverError> {
    let Some(bus_type) = bus_type_for_desc(&desc) else {
        requeue_desc(desc.id());
        return Err(DriverError::InvalidInput);
    };

    let Some(device) = device_registry().find_device(desc.device_id()) else {
        requeue_desc(desc.id());
        return Err(DriverError::BadState);
    };

    let drivers = match candidates {
        ProbeCandidates::BusType => bus_type.drivers_snapshot(),
        ProbeCandidates::Only { driver } => alloc::vec![driver],
    };

    let mut probed = false;
    for driver in drivers {
        let ops = driver.ops();
        if matches!(
            bus_type.match_desc(ops.as_ref(), &desc),
            MatchResult::NoMatch
        ) {
            continue;
        }

        #[cfg(unittest)]
        if test_hooks::take_candidate_setup_failure() {
            requeue_desc(desc.id());
            return Err(DriverError::NoMemory);
        }

        // A brand-new runtime for every candidate; the object is never reused
        // with a reset runtime.
        if let Err(error) = device.install_fresh_binding() {
            requeue_desc(desc.id());
            return Err(error);
        }

        // The driver's probe may construct class-layer state that reads the
        // binding metadata, but subscribers must not see it before the
        // registry and bus publish the device.
        prepare_device_binding(
            device.as_ref(),
            driver.id(),
            driver.name(),
            driver.device_kind(),
        );

        // Account every real probe attempt on both the driver and its bus
        // instance so repeated failures stay observable.
        let bus_instance = device_registry().find_bus(desc.bus_id());
        driver.record_probe_attempt();
        if let Some(bus) = &bus_instance {
            bus.record_probe_attempt();
        }
        probed = true;

        match ops.probe_device(device.clone()) {
            Ok(()) => {
                publish_desc_device(desc.id(), device.clone(), driver.clone());
                // Compatible late-parent attach: registration may have run
                // before the parent was registered. Existing topology is never
                // rebuilt or cleared here.
                if device.parent().is_none()
                    && let Some(parent_id) = desc.parent()
                {
                    // Release the registry guard before the topology helper
                    // takes it again for its own snapshot.
                    let parent_registered = device_registry().find_device(parent_id).is_some();
                    if parent_registered
                        && let Err(err) = attach_device_parent_locked(parent_id, device.id())
                    {
                        log::warn!(
                            "failed to attach device {:?} under parent {:?}: {:?}",
                            device.id(),
                            parent_id,
                            err,
                        );
                    }
                }
                driver.attach_device(device.id());
                announce_device_matched(device.as_ref());
                announce_device_bound(device.as_ref(), driver.device_kind());
                dispatch_device_event(DeviceEvent::Published { id: device.id() });
                activate_device(device.as_ref(), driver.device_kind());
                return Ok(ProbeOutcome::Activated);
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
                // Retire this binding instance completely before the next
                // candidate: terminal state, cleanup batch, instance-scoped
                // event, then clear the slot. The stable object stays.
                device.detach_driver();
                device.mark_removed();
                device.run_cleanups();
                dispatch_device_event(DeviceEvent::BindingRetired { id: device.id() });
                device.clear_binding();
            }
        }
    }

    requeue_desc(desc.id());
    Ok(if probed {
        ProbeOutcome::Unclaimed
    } else {
        ProbeOutcome::Unmatched
    })
}

/// Probe one registered descriptor through the descriptor-first driver path.
///
/// The caller must be in a sleepable task context: cross-thread contention is
/// resolved by blocking on the descriptor's binding lock and re-reading its
/// state, not by returning [`ProbeOutcome::Busy`]. `Busy` only reports
/// same-context recursion (for example a probe probing its own descriptor).
pub fn probe_device_desc(id: DeviceDescId) -> Result<ProbeOutcome, DriverError> {
    let binding = device_registry()
        .binding_for_desc(id)
        .ok_or(DriverError::InvalidInput)?;
    if binding.is_owned_by_current() {
        return Ok(ProbeOutcome::Busy);
    }

    let _guard = binding.lock();
    #[cfg(unittest)]
    test_hooks::pause(test_hooks::BindingPause::ProbeHoldingLock { desc_id: id });
    match inspect_locked_desc(id)? {
        None => Ok(ProbeOutcome::Skipped),
        Some(desc) => {
            if device_registry().mark_device_desc_probing(id).is_none() {
                return Err(DriverError::BadState);
            }
            probe_desc_round(desc, ProbeCandidates::BusType)
        }
    }
}

/// Adopt a device that boot code already initialized, skipping descriptor match/probe.
///
/// This path is for explicit boot/runtime handoff contracts. It creates a
/// stable device object, registers it in the driver-core registry, installs one
/// fresh binding runtime and records the driver binding metadata. It drives the
/// same canonical lifecycle event sequence as the probe path (`Matched` ->
/// `Bound` -> `Published` -> `Activated`) so that subscribers see adopted
/// devices (e.g. PCI host bridges, PCI-PCI bridges) the same way they see
/// probed ones; when adoption runs before any subscriber is registered the
/// dispatches are simply no-ops. The caller is responsible for completing
/// subsystem-level registration (e.g. `publish_char`).
///
/// Publication holds the new descriptor's binding lock from before the device
/// is visible until `Activated`. Because adoption also runs under the bus
/// manager spinlock, the lock is taken with a non-blocking `try_lock`; if the
/// attempt fails the device is not published and the call returns
/// [`DriverError::BadState`].
pub fn adopt_active_device(
    adoption: ActiveDeviceAdoption,
) -> Result<Arc<DeviceObject>, DriverError> {
    init_device_registry();

    let driver = adoption.driver;
    // Validate the target bus and mint the IDs under the registry lock, then
    // drop it before constructing the (heap-allocating) descriptor, lock and
    // object so allocation does not run under the lock.
    let (bus_instance, desc_id, device_id) = {
        let mut index = device_registry();
        let bus_instance = index.find_bus(adoption.bus_id);
        let bus = bus_instance.as_ref().ok_or(DriverError::InvalidInput)?;
        if !driver.bus_types().contains(&bus.bus_type()) {
            return Err(DriverError::InvalidInput);
        }
        (
            bus_instance,
            index.alloc_device_desc_id(),
            index.alloc_device_id(),
        )
    };

    let binding = Arc::new(Mutex::new(()));
    let desc = DeviceDesc::from_spec(
        desc_id,
        device_id,
        DeviceDescSpec {
            bus_id: adoption.bus_id,
            parent: adoption.parent,
            location: adoption.location,
            origin: adoption.origin,
            identity: adoption.identity,
            transport: adoption.transport,
            resources: adoption.resources.clone(),
            dma: adoption.dma,
            intx: None,
            firmware_node: None,
        },
    );
    let device = Arc::new(
        DeviceObject::new_registered(
            device_id,
            adoption.bus_id,
            adoption.location,
            adoption.origin,
            adoption.identity,
            adoption.transport,
            adoption.resources,
            binding.clone(),
        )
        .with_dma(adoption.dma)
        .with_firmware_node(None),
    );

    // The entry is not published yet, so nobody else can hold or contend for
    // this lock. `try_lock` keeps adoption non-blocking while it runs inside
    // the bus-manager spinlock; failure is a broken invariant, not a race.
    let _binding_guard = binding.try_lock().ok_or(DriverError::BadState)?;

    // Prepare the binding metadata before the object becomes visible. From
    // this point on a concurrent remove can safely inspect `driver_id()` once
    // it finds the device on the bus.
    device.install_fresh_binding()?;
    prepare_device_binding(
        device.as_ref(),
        driver.id(),
        driver.name(),
        driver.device_kind(),
    );

    {
        let mut index = device_registry();
        if !index.add_device_desc_with_binding(desc, binding.clone()) {
            return Err(DriverError::BadState);
        }
        index.add_device(device.clone());
        index.bind_device_desc(desc_id);
    }

    // Bus instance must be mutated outside the registry guard per the
    // lock-order discipline. Metadata is already set (see above).
    if let Some(bus) = bus_instance {
        bus.attach_bound_device(device.clone(), driver.clone());
    }

    if let Some(parent_id) = adoption.parent {
        // Release the registry guard before the topology helper takes it again.
        let parent_registered = device_registry().find_device(parent_id).is_some();
        if parent_registered && let Err(err) = attach_device_parent_locked(parent_id, device.id()) {
            log::warn!(
                "failed to attach adopted device {:?} under parent {:?}: {:?}",
                device.id(),
                parent_id,
                err,
            );
        }
    }
    driver.attach_device(device.id());

    #[cfg(unittest)]
    test_hooks::pause(test_hooks::BindingPause::AdoptionBeforeActivate {
        device_id: device.id(),
    });

    // Announce the canonical sequence only after the device is queryable, and
    // hold the descriptor lock through activation so a remove cannot commit
    // inside the publication window.
    announce_device_matched(device.as_ref());
    announce_device_bound(device.as_ref(), driver.device_kind());
    dispatch_device_event(DeviceEvent::Published { id: device.id() });
    activate_device(device.as_ref(), driver.device_kind());

    Ok(device)
}

/// Look up the descriptor binding lock that serializes writes for `device_id`.
fn binding_for_device(device_id: DeviceId) -> Result<Arc<Mutex<()>>, DriverError> {
    let index = device_registry();
    if index.find_device(device_id).is_none() {
        return Err(DriverError::InvalidInput);
    }
    let desc = index
        .descriptor_for_device(device_id)
        .ok_or(DriverError::BadState)?;
    index
        .binding_for_desc(desc.id())
        .ok_or(DriverError::BadState)
}

/// Attach a child device under a parent device object.
///
/// Serialized by the child descriptor binding lock (or the enclosing write
/// context when already held). All touched relations are locked in ascending
/// `DeviceId` order; self-parent and cycles are rejected with
/// `InvalidInput`. An existing parent is atomically replaced.
pub fn attach_device_parent(parent_id: DeviceId, child_id: DeviceId) -> Result<(), DriverError> {
    if parent_id == child_id {
        return Err(DriverError::InvalidInput);
    }
    let binding = binding_for_device(child_id)?;
    if binding.is_owned_by_current() {
        return attach_device_parent_locked(parent_id, child_id);
    }
    let _guard = binding.lock();
    attach_device_parent_locked(parent_id, child_id)
}

/// Detach a child device from its current parent.
///
/// Serialized by the child descriptor binding lock. Only the relation that is
/// still equal to the observed parent is cleared.
pub fn detach_device_parent(child_id: DeviceId) -> Result<(), DriverError> {
    let binding = binding_for_device(child_id)?;
    if binding.is_owned_by_current() {
        return detach_device_parent_locked(child_id);
    }
    let _guard = binding.lock();
    detach_device_parent_locked(child_id)
}

/// What happens to the discovery descriptor when its bound device is removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DescDisposition {
    /// Unbind: restore `Pending` so the descriptor may be bound again.
    Requeue,
    /// Unregister: delete the descriptor registration for good.
    Delete,
}

/// Remove or requeue a device after the binding has been committed for removal.
///
/// The caller holds the descriptor's binding lock and must have committed the
/// removal with `DeviceObject::begin_removing`.
///
/// `Requeue` keeps the stable object and its topology in the registry, restores
/// the descriptor to `Pending`, dispatches `Removed` while the slot still holds
/// the terminal binding, and only then clears the slot (`Discovered`).
///
/// `Delete` removes the descriptor and the device index entry together,
/// terminates the registration, conditionally clears the object's topology
/// relations, and dispatches exactly one `Removed`.
fn remove_device_from_index(
    desc_id: DeviceDescId,
    id: DeviceId,
    disposition: DescDisposition,
) -> Result<(), DriverError> {
    // Lock-order discipline: take Arc snapshots under the registry guard,
    // drop the guard, then mutate per-object state. The topology snapshot is
    // taken in the same critical section as the index deletion so the
    // registration path's "link then recheck" is mutually visible with it.
    let (device, bus, driver, parent, children, disposition_applied) = {
        let mut index = device_registry();
        let Some(device) = index.find_device(id) else {
            // Unreachable for both callers: they rechecked under the binding
            // lock immediately before committing the removal.
            return match disposition {
                DescDisposition::Requeue => Ok(()),
                DescDisposition::Delete => Err(DriverError::BadState),
            };
        };
        let bus = index.find_bus(device.bus_id());
        let driver = device
            .driver_id()
            .and_then(|driver_id| index.find_driver(driver_id));
        let (parent, children) = device.topology_snapshot();
        let disposition_applied = match disposition {
            DescDisposition::Requeue => index.requeue_device_desc_if_bound(desc_id, id),
            DescDisposition::Delete => {
                index.remove_device(id);
                index
                    .remove_device_desc_if_state(desc_id, DeviceDescState::Bound)
                    .is_some()
            }
        };
        if !disposition_applied {
            log::error!(
                "descriptor {:?} did not match the expected disposition for removed device {:?}",
                desc_id,
                id,
            );
        }
        (device, bus, driver, parent, children, disposition_applied)
    };

    #[cfg(unittest)]
    test_hooks::pause(test_hooks::BindingPause::RemoveAfterRequeue {
        desc_id,
        device_id: id,
    });

    // Detach the device and opportunistically orphan the shared driver entry
    // in one bus critical section. Metadata is still present, which is what
    // lets the bus decide whether another device still uses this driver.
    if let Some(bus) = &bus {
        bus.detach_device_and_orphan_driver(id, driver.as_ref().map(|driver| driver.id()));
    }
    if let Some(driver) = driver {
        driver.detach_device(id);
    }
    // Clear bound-driver metadata only after the bus no longer lists the
    // device. Topology is preserved on unbind and removed on unregister.
    device.detach_driver();
    device.mark_removed();

    match disposition {
        DescDisposition::Requeue => {
            dispatch_device_event(DeviceEvent::Removed { id });
            // The slot still held the terminal A while subscribers revoked
            // their captured handles; only now may the stable object report
            // Discovered and accept a fresh runtime.
            device.clear_binding();
        }
        DescDisposition::Delete => {
            device.mark_unregistered();
            #[cfg(unittest)]
            test_hooks::pause(test_hooks::BindingPause::UnregisterBeforeTopologyCleanup {
                device_id: id,
            });
            clear_registration_topology(&device, parent, children);
            dispatch_device_event(DeviceEvent::Removed { id });
        }
    }

    if disposition_applied || disposition == DescDisposition::Requeue {
        Ok(())
    } else {
        // Unregistration could not delete the registration it just tore down.
        // The device is already irreversibly removed, so report the broken
        // invariant instead of pretending the descriptor no longer exists.
        Err(DriverError::BadState)
    }
}

/// Unregister a discovery descriptor and tear down any device bound from it.
///
/// Unlike [`remove_device_managed`], this function removes the descriptor's
/// registration instead of restoring it to `Pending`: after it succeeds the
/// old [`DeviceDescId`] and stable [`DeviceId`] cannot be probed, matched, or
/// rebound, even through a scan snapshot taken before the call. Re-discovery
/// must allocate a new descriptor and device id. A `Pending` descriptor is
/// deleted together with its (unbound) stable object without emitting any
/// event; a `Bound` one runs the normal remove sequence and emits exactly one
/// `Removed`.
///
/// The caller must be in a sleepable task context and must not already hold
/// this descriptor's binding lock (for example from a probe or `Removed`
/// subscriber); that recursion returns [`DriverError::ResourceBusy`] instead
/// of deadlocking. A concurrent unregister or unbind that wins while this
/// call waits for the lock is a legal race: if the registration is already
/// gone the call returns `Ok(())` without further teardown.
///
/// [`DriverError::ResourceBusy`] from the commit point means no teardown was
/// committed: the descriptor and binding are unchanged, and the caller may
/// retry later. [`DriverError::BadState`] means an internal lifecycle
/// invariant did not hold; when it is reported after the commit point the
/// device has still been torn down and must not be treated as usable.
pub fn unregister_device_desc(id: DeviceDescId) -> Result<(), DriverError> {
    let binding = device_registry()
        .binding_for_desc(id)
        .ok_or(DriverError::InvalidInput)?;
    if binding.is_owned_by_current() {
        // `kmutex::Mutex` panics on recursive acquisition. Reject same-context
        // re-entry before blocking; a different task waits below.
        return Err(DriverError::ResourceBusy);
    }

    let _guard = binding.lock();
    #[cfg(unittest)]
    test_hooks::pause(test_hooks::BindingPause::UnregisterHoldingLock { desc_id: id });

    enum Target {
        Absent,
        Pending,
        Probing,
        Bound(DeviceId),
    }

    // Read the state and its backing object in one registry critical section,
    // like `inspect_locked_desc`. The binding lock excludes every writer, so
    // this decision is stable until the deletion below (which rechecks it).
    let target = {
        let index = device_registry();
        match index.device_desc_state(id) {
            None => Target::Absent,
            Some(DeviceDescState::Pending) => Target::Pending,
            Some(DeviceDescState::Probing) => Target::Probing,
            Some(DeviceDescState::Bound) => {
                let desc = index.find_device_desc(id).ok_or(DriverError::BadState)?;
                let device_id = desc.device_id();
                // 1:1 invariant: exactly this descriptor owns the stable object.
                if index.find_device(device_id).is_none()
                    || index
                        .descriptor_bound_to_device(device_id)
                        .map(|desc_ref| desc_ref.id)
                        != Some(id)
                {
                    return Err(DriverError::BadState);
                }
                Target::Bound(device_id)
            }
        }
    };

    match target {
        // Another unregister (or an unbind followed by one) removed the
        // registration while this task waited. No teardown is left to do.
        Target::Absent => Ok(()),
        // Only the binding-lock holder may leave a descriptor in `Probing`;
        // seeing it here means the lifecycle invariant is broken.
        Target::Probing => Err(DriverError::BadState),
        Target::Pending => {
            // Delete the descriptor and the never-bound stable object together;
            // no event is emitted because no binding was ever published.
            let (desc, device) = {
                let mut index = device_registry();
                let desc = index.remove_device_desc_if_state(id, DeviceDescState::Pending);
                let device = desc
                    .as_ref()
                    .and_then(|desc| index.remove_device(desc.device_id()));
                (desc, device)
            };
            let Some(_) = desc else {
                log::error!("descriptor {:?} was not Pending under its binding lock", id);
                return Err(DriverError::BadState);
            };
            if let Some(device) = device {
                let (parent, children) = device.topology_snapshot();
                #[cfg(unittest)]
                test_hooks::pause(test_hooks::BindingPause::UnregisterBeforeTopologyCleanup {
                    device_id: device.id(),
                });
                device.mark_unregistered();
                clear_registration_topology(&device, parent, children);
            }
            Ok(())
        }
        Target::Bound(device_id) => {
            remove_bound_device_locked(id, device_id, DescDisposition::Delete, None)
        }
    }
}

/// Commit and run the removal sequence for a descriptor/device binding.
///
/// The caller holds the binding lock and has resolved `desc_id` to a live
/// `device_id`. Everything is re-read after the wait; an unbind that finds
/// the device already gone returns [`DriverError::ResourceBusy`], while an
/// unregister treats that as an invariant break.
fn remove_bound_device_locked(
    desc_id: DeviceDescId,
    device_id: DeviceId,
    disposition: DescDisposition,
    expected_binding: Option<&BindingRef>,
) -> Result<(), DriverError> {
    // Re-read everything after the wait; the snapshots taken before it may be
    // stale. A device that disappeared here was removed by another task.
    let (device, driver, bus_type) = {
        let index = device_registry();
        let Some(device) = index.find_device(device_id) else {
            return match disposition {
                DescDisposition::Requeue => Err(DriverError::ResourceBusy),
                DescDisposition::Delete => Err(DriverError::BadState),
            };
        };
        if !index.device_desc_is_bound_to(desc_id, device_id) {
            return match disposition {
                DescDisposition::Requeue => Err(DriverError::ResourceBusy),
                DescDisposition::Delete => Err(DriverError::BadState),
            };
        }
        let driver = device
            .driver_id()
            .and_then(|driver_id| index.find_driver(driver_id));
        let bus_type = index
            .find_bus(device.bus_id())
            .map(|bus| index.find_bus_type(bus.bus_type()));
        (device, driver, bus_type)
    };

    // The pre-wait snapshot named one binding instance, but another task may
    // have requeued the descriptor and installed a replacement while this task
    // waited. Recheck the instance, not just the stable device id: a stale
    // remove must not tear down a binding it never observed.
    if matches!(disposition, DescDisposition::Requeue) && expected_binding.is_none() {
        // The pre-lock observation could not capture a stable instance: a
        // removal or Busy backoff was in flight. The caller may retry; do not
        // report this legal race as the non-retryable BadState.
        return Err(DriverError::ResourceBusy);
    }
    if let Some(expected) = expected_binding {
        let Some(current) = device.binding_ref() else {
            return Err(DriverError::BadState);
        };
        if !current.same_binding(expected) {
            return Err(DriverError::ResourceBusy);
        }
    }

    // `begin_removing` is the single commit point: it atomically claims the
    // binding for removal (or fails with `ResourceBusy` if another remove is
    // already in flight or references are outstanding). It must run after the
    // descriptor lock is acquired, so an in-flight publish cannot mark the
    // binding active after we commit.
    if !device.begin_removing() {
        return Err(DriverError::ResourceBusy);
    }

    // Past the commit point removal is irreversible. `driver.remove` and
    // `bus_type.remove` are best-effort cleanup hooks; once `driver.remove`
    // has run the device's private state (MMIO mappings, IRQ handlers, queues)
    // may already be gone, so we must NOT roll the lifecycle back to a usable
    // state on failure — doing so would expose a torn-down binding as `Active`
    // and invite use-after-free. Failures are logged and removal proceeds to
    // `Removed`. This mirrors the "remove never fails" contract of Linux's
    // driver core.
    if let Some(driver) = driver
        && let Err(error) = driver.ops().remove(device.clone())
    {
        log::error!(
            "driver {} remove() failed for device {:?}: {:?}; proceeding with removal",
            driver.name(),
            device_id,
            error,
        );
    }

    if let Some(bus_type) = bus_type
        && let Err(error) = bus_type.remove(device.clone())
    {
        log::error!(
            "bus-type remove() failed for device {:?}: {:?}; proceeding with removal",
            device_id,
            error,
        );
    }

    // Release any driver-registered resources that outlived the driver's own
    // remove() hook. Runs after both teardown hooks so the driver gets first
    // chance to release things itself.
    device.run_cleanups();

    remove_device_from_index(desc_id, device_id, disposition)
}

/// Run the driver-core remove path for a live device.
///
/// The caller must be in a sleepable task context. Removal serializes with
/// binding through the unique descriptor's binding lock: while this function
/// waits, an in-flight probe or adoption finishes first, and only after this
/// removal releases the lock (after `Removed`) may a new binding start. The
/// stable object, its facts and its topology survive; only the current binding
/// is retired.
///
/// [`DriverError::ResourceBusy`] means this call did not commit a teardown:
/// another remover may already have won, the binding may have been replaced
/// while this call waited for the descriptor lock, or references to the
/// device are still outstanding. The caller may retry later; no eventual
/// completion is guaranteed.
pub fn remove_device_managed(id: DeviceId) -> Result<(), DriverError> {
    // Locate the unique descriptor backing this device before waiting. Zero or
    // multiple descriptors mean the 1:1 invariant is broken; do not touch the
    // device in that case.
    let (desc_ref, device) = {
        let index = device_registry();
        let device = index.find_device(id).ok_or(DriverError::InvalidInput)?;
        let desc_ref = index
            .descriptor_bound_to_device(id)
            .ok_or(DriverError::BadState)?;
        (desc_ref, device)
    };
    // A transient Removing state makes this capture fail; keep the wait and
    // classify the race as retryable ResourceBusy after the lock instead.
    let observed_binding = device.binding_ref();

    let _guard = desc_ref.binding.lock();
    remove_bound_device_locked(
        desc_ref.id,
        id,
        DescDisposition::Requeue,
        observed_binding.as_ref(),
    )
}

unittest::path_mod_test!(tests, "../../ut-tests/lifecycle.rs");
