// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cross-task topology and registration interleavings.
//!
//! These tests need real kernel tasks (`ktask`) and therefore live in
//! `kdriver`; `kdevice` itself has no task-layer dependency. Every pause uses
//! either a blocking yield gate in a task context (pause hooks) or a bounded
//! atomic handshake while relation spinlocks are held (relations-lock hook).

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use driver_base::DriverError;
use kdevice::test_hooks::{self, BindingPause};
use kdevice::{
    self, BusInstance, DeviceDescId, DeviceDescSpec, DeviceId, DeviceIdentity,
    DeviceLocation, DiscoveryOrigin, PlatformIdentity, ProbeOutcome, ResourceSet, device_desc_add,
    find_device, probe_device_desc, unregister_device_desc,
};
use kspin::SpinNoPreempt;
use unittest::{assert, assert_eq, def_test};

fn setup() -> Arc<BusInstance> {
    test_hooks::clear_pause_hook();
    test_hooks::clear_relations_lock_hook();
    kdevice::reset_and_setup_platform_bus_for_tests("topology-thread-bus").1
}

fn desc(bus: &BusInstance, location: u16, parent: Option<DeviceId>) -> DeviceDescSpec {
    DeviceDescSpec {
        bus_id: bus.id(),
        parent,
        location: DeviceLocation::PlatformStatic { id: location },
        origin: DiscoveryOrigin::PlatformStatic,
        identity: DeviceIdentity::Platform(PlatformIdentity {
            alias: Some("topology-thread-device"),
            firmware_id: None,
        }),
        transport: None,
        resources: ResourceSet::new(),
        dma: None,
        intx: None,
        firmware_node: None,
    }
}

fn spawn(name: &'static str, task: impl FnOnce() + Send + 'static) -> ktask::KtaskRef {
    ktask::spawn_task(ktask::TaskInner::new_kthread(task, name.into(), 0x8000).unwrap())
}

/// Yield iterations allowed for a peer to reach a pause. A test that never
/// reaches its handshake fails instead of hanging the whole runner.
const WAIT_YIELDS: usize = 2_000_000;

/// Bounded spin iterations used by the reverse-attach handshake.
const HANDSHAKE_SPINS: usize = 20_000_000;

/// Wait until `task` blocks, fails if it exits, and gives up after
/// [`WAIT_YIELDS`] iterations. Returns the observed state on failure so the
/// caller can report it after releasing its gate and joining the task.
fn wait_until_blocked(task: &ktask::KtaskRef) -> Result<(), ktask::TaskState> {
    for _ in 0..WAIT_YIELDS {
        match task.state() {
            ktask::TaskState::Blocked => return Ok(()),
            ktask::TaskState::Exited => return Err(ktask::TaskState::Exited),
            _ => ktask::yield_now(),
        }
    }
    Err(task.state())
}

/// Wait until `flag` is set. Returns `false` if the pause was never reached.
fn wait_for(flag: &AtomicBool) -> bool {
    for _ in 0..WAIT_YIELDS {
        if flag.load(Ordering::SeqCst) {
            return true;
        }
        ktask::yield_now();
    }
    false
}

/// A blocking gate used from a pause hook.
struct Gate {
    reached: AtomicBool,
    release: AtomicBool,
}

impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            reached: AtomicBool::new(false),
            release: AtomicBool::new(false),
        })
    }

    fn wait(&self) {
        self.reached.store(true, Ordering::SeqCst);
        while !self.release.load(Ordering::SeqCst) {
            ktask::yield_now();
        }
    }
}

fn desc_id_for_device(device_id: DeviceId) -> Option<DeviceDescId> {
    kdevice::device_descs_snapshot()
        .into_iter()
        .find(|desc| desc.device_id() == device_id)
        .map(|desc| desc.id())
}

#[def_test(serial)]
fn test_probe_waits_for_registration_binding_lock() -> unittest::TestResult {
    let bus = setup();
    let parent = device_desc_add(desc(&bus, 1, None)).unwrap();
    let gate = Gate::new();
    let child_cell = Arc::new(SpinNoPreempt::new(None::<DeviceId>));
    {
        let gate = gate.clone();
        let child_cell = child_cell.clone();
        test_hooks::install_pause_hook(Arc::new(move |point| {
            if let BindingPause::TopologyBeforeLink { child_id, .. } = point {
                *child_cell.lock() = Some(child_id);
                gate.wait();
            }
        }));
    }

    let bus_id = bus.id();
    let parent_id = parent.device_id();
    let registration = spawn("register-under-lock", move || {
        device_desc_add(desc_spec_for(bus_id, 2, Some(parent_id))).unwrap();
    });
    let paused = wait_for(&gate.reached);
    // Observe everything that can fail before the gate is released.
    let child_device_id = child_cell.lock().take();
    let desc_id = child_device_id.and_then(desc_id_for_device);
    if !paused || desc_id.is_none() {
        gate.release.store(true, Ordering::SeqCst);
        let _ = registration.join();
        test_hooks::clear_pause_hook();
        assert!(paused, "the registration never reached the topology pause");
        assert!(
            desc_id.is_some(),
            "the pause did not publish a descriptor for the child"
        );
        return unittest::TestResult::Failed;
    }
    let desc_id = desc_id.unwrap();

    // The concurrent probe must wait for the registration's fresh binding lock
    // before it can inspect the descriptor.
    let outcome_cell = Arc::new(SpinNoPreempt::new(None));
    let probe_cell = outcome_cell.clone();
    let probe = spawn("probe-during-registration", move || {
        let outcome = probe_device_desc(desc_id).unwrap();
        *probe_cell.lock() = Some(outcome);
    });
    let probe_blocked = wait_until_blocked(&probe);

    gate.release.store(true, Ordering::SeqCst);
    let registration_joined = registration.join();
    let probe_joined = probe.join();
    test_hooks::clear_pause_hook();
    assert_eq!(registration_joined, 0);
    assert_eq!(probe_joined, 0);
    assert!(
        probe_blocked.is_ok(),
        "the probe did not park on the registration's binding lock: {probe_blocked:?}"
    );
    assert_eq!(
        outcome_cell.lock().take(),
        Some(ProbeOutcome::Unmatched),
        "the probe ran after publication instead of skipping the descriptor"
    );
    assert!(!test_hooks::binding_is_owned_by_current(desc_id));
    unittest::TestResult::Ok
}

fn desc_spec_for(
    bus_id: kdevice::BusId,
    location: u16,
    parent: Option<DeviceId>,
) -> DeviceDescSpec {
    DeviceDescSpec {
        bus_id,
        parent,
        location: DeviceLocation::PlatformStatic { id: location },
        origin: DiscoveryOrigin::PlatformStatic,
        identity: DeviceIdentity::Platform(PlatformIdentity {
            alias: Some("topology-thread-device"),
            firmware_id: None,
        }),
        transport: None,
        resources: ResourceSet::new(),
        dma: None,
        intx: None,
        firmware_node: None,
    }
}

#[def_test(serial)]
fn test_registration_holds_fresh_lock_through_initial_link() {
    let bus = setup();
    let parent = device_desc_add(desc(&bus, 1, None)).unwrap();

    let desc_cell = Arc::new(SpinNoPreempt::new(None::<DeviceDescId>));
    let owned_at_link = Arc::new(AtomicBool::new(false));
    {
        let desc_cell = desc_cell.clone();
        let owned_at_link = owned_at_link.clone();
        test_hooks::install_pause_hook(Arc::new(move |point| match point {
            BindingPause::RegistrationHoldingLock { desc_id } => {
                *desc_cell.lock() = Some(desc_id);
            }
            BindingPause::TopologyBeforeLink { .. } => {
                if let Some(desc_id) = *desc_cell.lock() {
                    // The link pause runs after publication, so the registry
                    // can now answer which task owns the fresh binding lock.
                    owned_at_link.store(
                        test_hooks::binding_is_owned_by_current(desc_id),
                        Ordering::SeqCst,
                    );
                }
            }
            _ => {}
        }));
    }

    // Simulate the enumeration write context: a spin lock is already held
    // across registration, so the registration path must not call a sleeping
    // `lock()` on its fresh descriptor lock.
    let enumeration = SpinNoPreempt::new(());
    let enumerator = enumeration.lock();
    let child = device_desc_add(desc(&bus, 2, Some(parent.device_id()))).unwrap();
    drop(enumerator);
    test_hooks::clear_pause_hook();

    assert!(
        owned_at_link.load(Ordering::SeqCst),
        "the registration still owns its fresh binding lock at the initial link"
    );
    let desc_id = desc_cell.lock().take().unwrap();
    assert!(
        !test_hooks::binding_is_owned_by_current(desc_id),
        "registration releases the fresh lock before returning"
    );
    let parent_device = find_device(parent.device_id()).unwrap();
    let child_device = find_device(child.device_id()).unwrap();
    assert_eq!(child_device.parent(), Some(parent.device_id()));
    assert!(parent_device.children_snapshot().contains(&child.device_id()));
}

#[def_test(serial)]
fn test_parent_unregister_before_initial_link_is_rolled_back() -> unittest::TestResult {
    let bus = setup();
    let parent = device_desc_add(desc(&bus, 1, None)).unwrap();
    let parent_device = find_device(parent.device_id()).unwrap();
    let gate = Gate::new();
    {
        let gate = gate.clone();
        test_hooks::install_pause_hook(Arc::new(move |point| {
            if let BindingPause::TopologyBeforeLink { .. } = point {
                gate.wait();
            }
        }));
    }

    let parent_desc_id = parent.id();
    let parent_device_id = parent.device_id();
    let bus_id = bus.id();
    let registration = spawn("register-child-before-link", move || {
        device_desc_add(desc_spec_for(bus_id, 2, Some(parent_device_id))).unwrap();
    });
    let paused = wait_for(&gate.reached);

    // Observe while the registration is paused; every fallible step is stored
    // so the gate can be released and the task joined before asserting.
    let child_id = find_device_by_parent(parent.device_id());
    let unregister_result = unregister_device_desc(parent_desc_id);
    let parent_gone = find_device(parent.device_id()).is_none();

    gate.release.store(true, Ordering::SeqCst);
    let joined = registration.join();
    test_hooks::clear_pause_hook();

    assert!(paused, "the registration never reached the topology pause");
    assert!(
        child_id.is_some(),
        "the paused registration published no child"
    );
    let child_id = child_id.unwrap();
    assert_eq!(joined, 0);
    assert!(unregister_result.is_ok(), "the parent unregister failed");
    assert!(parent_gone, "the parent object was not removed");

    let child = find_device(child_id).expect("the child registration survives");
    assert_eq!(child.parent(), None, "the stale link is conditionally undone");
    // The parent object is already unregistered, so the rollback touches only
    // the still-live child; a dead parent object may retain the stale id, but
    // nothing reachable through the registry points at it.
    assert!(find_device(parent.device_id()).is_none());
    assert!(
        parent_device.children_snapshot().iter().all(|child_id| {
            find_device(*child_id)
                .is_none_or(|device| device.parent() != Some(parent.device_id()))
        }),
        "no live child points at the removed parent"
    );
    unittest::TestResult::Ok
}

/// Locate the child registered under `parent` via the public descriptor view.
fn find_device_by_parent(parent: DeviceId) -> Option<DeviceId> {
    kdevice::device_descs_snapshot()
        .into_iter()
        .find(|desc| desc.parent() == Some(parent))
        .map(|desc| desc.device_id())
}

#[def_test(serial)]
fn test_parent_unregister_after_link_is_cleaned() -> unittest::TestResult {
    let bus = setup();
    let parent = device_desc_add(desc(&bus, 1, None)).unwrap();
    let parent_device = find_device(parent.device_id()).unwrap();
    let gate = Gate::new();
    {
        let gate = gate.clone();
        test_hooks::install_pause_hook(Arc::new(move |point| {
            if let BindingPause::TopologyAfterLink { .. } = point {
                gate.wait();
            }
        }));
    }

    let parent_desc_id = parent.id();
    let parent_device_id = parent.device_id();
    let bus_id = bus.id();
    let registration = spawn("register-child-after-link", move || {
        device_desc_add(desc_spec_for(bus_id, 2, Some(parent_device_id))).unwrap();
    });
    let paused = wait_for(&gate.reached);

    let child_id = find_device_by_parent(parent.device_id());
    let unregister_result = unregister_device_desc(parent_desc_id);
    let parent_gone = find_device(parent.device_id()).is_none();

    gate.release.store(true, Ordering::SeqCst);
    let joined = registration.join();
    test_hooks::clear_pause_hook();

    assert!(paused, "the registration never reached the topology pause");
    assert!(
        child_id.is_some(),
        "the paused registration published no child"
    );
    let child_id = child_id.unwrap();
    assert_eq!(joined, 0);
    assert!(unregister_result.is_ok(), "the parent unregister failed");
    assert!(parent_gone, "the parent object was not removed");

    let child = find_device(child_id).expect("the child registration survives");
    assert_eq!(child.parent(), None, "unregister cleaned the linked edge");
    assert!(parent_device.children_snapshot().is_empty());
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn test_child_unregister_during_registration_blocks_then_rechecks() -> unittest::TestResult {
    let bus = setup();
    let parent = device_desc_add(desc(&bus, 1, None)).unwrap();
    let parent_device = find_device(parent.device_id()).unwrap();
    let gate = Gate::new();
    {
        let gate = gate.clone();
        test_hooks::install_pause_hook(Arc::new(move |point| {
            if let BindingPause::TopologyBeforeLink { .. } = point {
                gate.wait();
            }
        }));
    }

    let bus_id = bus.id();
    let parent_id = parent.device_id();
    let registration = spawn("register-child-blocking", move || {
        device_desc_add(desc_spec_for(bus_id, 2, Some(parent_id))).unwrap();
    });
    let paused = wait_for(&gate.reached);
    let child_id = find_device_by_parent(parent.device_id());
    let child_desc_id = child_id.and_then(desc_id_for_device);
    if !paused || child_desc_id.is_none() {
        gate.release.store(true, Ordering::SeqCst);
        let _ = registration.join();
        test_hooks::clear_pause_hook();
        assert!(paused, "the registration never reached the topology pause");
        assert!(
            child_desc_id.is_some(),
            "the pause did not publish a child descriptor"
        );
        return unittest::TestResult::Failed;
    }
    let child_id = child_id.unwrap();
    let child_desc_id = child_desc_id.unwrap();

    let unregister_done = Arc::new(AtomicBool::new(false));
    let done = unregister_done.clone();
    let unregister = spawn("unregister-child", move || {
        unregister_device_desc(child_desc_id).unwrap();
        done.store(true, Ordering::SeqCst);
    });
    let unregister_blocked = wait_until_blocked(&unregister);
    let done_before_release = unregister_done.load(Ordering::SeqCst);

    gate.release.store(true, Ordering::SeqCst);
    let registration_joined = registration.join();
    let unregister_joined = unregister.join();
    test_hooks::clear_pause_hook();

    assert_eq!(registration_joined, 0);
    assert_eq!(unregister_joined, 0);
    assert!(
        unregister_blocked.is_ok(),
        "the unregister did not park on the registration's binding lock: {unregister_blocked:?}"
    );
    assert!(
        !done_before_release,
        "the unregister must wait for the registration's binding lock"
    );
    assert!(find_device(child_id).is_none(), "the child registration ended");
    assert!(
        parent_device.children_snapshot().is_empty(),
        "no invalid relation survives the two writers"
    );
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn test_stale_parent_cleanup_preserves_new_parent() -> unittest::TestResult {
    let bus = setup();
    let old_parent = device_desc_add(desc(&bus, 1, None)).unwrap();
    let new_parent = device_desc_add(desc(&bus, 2, None)).unwrap();
    let child = device_desc_add(desc(&bus, 3, Some(old_parent.device_id()))).unwrap();
    let old_parent_device = find_device(old_parent.device_id()).unwrap();
    let new_parent_device = find_device(new_parent.device_id()).unwrap();
    assert_eq!(find_device(child.device_id()).unwrap().parent(), Some(old_parent.device_id()));

    let gate = Gate::new();
    {
        let gate = gate.clone();
        let old_id = old_parent.device_id();
        test_hooks::install_pause_hook(Arc::new(move |point| {
            if let BindingPause::UnregisterBeforeTopologyCleanup { device_id } = point
                && device_id == old_id
            {
                gate.wait();
            }
        }));
    }

    let old_desc_id = old_parent.id();
    let unregister = spawn("unregister-old-parent", move || {
        unregister_device_desc(old_desc_id).unwrap();
    });
    let paused = wait_for(&gate.reached);

    // Re-parent while the old parent's cleanup snapshot is still pending.
    let reparent = kdevice::attach_device_parent(new_parent.device_id(), child.device_id());

    gate.release.store(true, Ordering::SeqCst);
    let joined = unregister.join();
    test_hooks::clear_pause_hook();

    assert!(paused, "the unregister never reached the topology pause");
    assert_eq!(joined, 0);
    assert!(reparent.is_ok(), "re-parenting the child failed");

    let child_device = find_device(child.device_id()).unwrap();
    assert_eq!(child_device.parent(), Some(new_parent.device_id()));
    assert!(
        new_parent_device.children_snapshot().contains(&child.device_id()),
        "the new parent keeps its child"
    );
    assert!(
        old_parent_device.children_snapshot().is_empty(),
        "the atomic re-parent removed the child from the old parent"
    );
    assert!(find_device(old_parent.device_id()).is_none());
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn test_failed_reparent_restores_previous_parent() -> unittest::TestResult {
    let bus = setup();
    let old_parent = device_desc_add(desc(&bus, 1, None)).unwrap();
    let new_parent = device_desc_add(desc(&bus, 2, None)).unwrap();
    let child = device_desc_add(desc(&bus, 3, Some(old_parent.device_id()))).unwrap();
    let old_parent_device = find_device(old_parent.device_id()).unwrap();
    let new_parent_device = find_device(new_parent.device_id()).unwrap();
    assert_eq!(
        find_device(child.device_id()).unwrap().parent(),
        Some(old_parent.device_id())
    );

    let gate = Gate::new();
    let new_parent_id = new_parent.device_id();
    let child_id = child.device_id();
    {
        let gate = gate.clone();
        test_hooks::install_pause_hook(Arc::new(move |point| {
            if let BindingPause::TopologyBeforeLink {
                parent_id,
                child_id: paused_child,
            } = point
                && parent_id == new_parent_id
                && paused_child == child_id
            {
                gate.wait();
            }
        }));
    }

    let new_desc_id = new_parent.id();
    let attach = spawn("reparent-child", move || {
        let _ = kdevice::attach_device_parent(new_parent_id, child_id);
    });
    let paused = wait_for(&gate.reached);

    // Remove the new parent while the attach is paused before the link.
    let unregister_result = unregister_device_desc(new_desc_id);
    let new_parent_gone = find_device(new_parent_id).is_none();

    gate.release.store(true, Ordering::SeqCst);
    let joined = attach.join();
    test_hooks::clear_pause_hook();

    assert!(paused, "the re-parent never reached the topology pause");
    assert_eq!(joined, 0);
    assert!(unregister_result.is_ok(), "the new parent unregister failed");
    assert!(new_parent_gone, "the new parent object was not removed");

    let child_device = find_device(child_id).expect("the child remains registered");
    assert_eq!(
        child_device.parent(),
        Some(old_parent.device_id()),
        "a failed re-parent must restore the previous parent edge"
    );
    assert!(
        old_parent_device.children_snapshot().contains(&child_id),
        "the old parent must keep its child after rollback"
    );
    assert!(
        new_parent_device.children_snapshot().is_empty(),
        "the failed new parent must not retain the child"
    );
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn test_failed_reparent_restores_previous_parent_after_link() -> unittest::TestResult {
    let bus = setup();
    let old_parent = device_desc_add(desc(&bus, 1, None)).unwrap();
    let new_parent = device_desc_add(desc(&bus, 2, None)).unwrap();
    let child = device_desc_add(desc(&bus, 3, Some(old_parent.device_id()))).unwrap();
    let old_parent_device = find_device(old_parent.device_id()).unwrap();
    let new_parent_device = find_device(new_parent.device_id()).unwrap();

    let gate = Gate::new();
    let new_parent_id = new_parent.device_id();
    let child_id = child.device_id();
    {
        let gate = gate.clone();
        test_hooks::install_pause_hook(Arc::new(move |point| {
            if let BindingPause::TopologyAfterLink {
                parent_id,
                child_id: paused_child,
            } = point
                && parent_id == new_parent_id
                && paused_child == child_id
            {
                gate.wait();
            }
        }));
    }

    let new_desc_id = new_parent.id();
    let attach = spawn("reparent-child-after-link", move || {
        let _ = kdevice::attach_device_parent(new_parent_id, child_id);
    });
    let paused = wait_for(&gate.reached);

    // The new parent's cleanup now sees the linked child and clears its parent
    // edge before the attach's rollback runs.
    let unregister_result = unregister_device_desc(new_desc_id);
    let new_parent_gone = find_device(new_parent_id).is_none();

    gate.release.store(true, Ordering::SeqCst);
    let joined = attach.join();
    test_hooks::clear_pause_hook();

    assert!(paused, "the re-parent never reached the after-link pause");
    assert_eq!(joined, 0);
    assert!(unregister_result.is_ok(), "the new parent unregister failed");
    assert!(new_parent_gone, "the new parent object was not removed");

    let child_device = find_device(child_id).expect("the child remains registered");
    assert_eq!(
        child_device.parent(),
        Some(old_parent.device_id()),
        "rollback must restore the old parent after the new parent cleanup"
    );
    assert!(
        old_parent_device.children_snapshot().contains(&child_id),
        "the old parent must keep its child after rollback"
    );
    assert!(
        new_parent_device.children_snapshot().is_empty(),
        "the failed new parent must not retain the child"
    );
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn test_explicit_attach_detach_serialized_by_child_lock() -> unittest::TestResult {
    let bus = setup();
    let parent = device_desc_add(desc(&bus, 1, None)).unwrap();
    let child = device_desc_add(desc(&bus, 2, None)).unwrap();
    let parent_device = find_device(parent.device_id()).unwrap();
    let gate = Gate::new();
    {
        let gate = gate.clone();
        let parent_id = parent.device_id();
        test_hooks::install_pause_hook(Arc::new(move |point| {
            if let BindingPause::TopologyBeforeLink { parent_id: linked, .. } = point
                && linked == parent_id
            {
                gate.wait();
            }
        }));
    }

    let parent_id = parent.device_id();
    let child_id = child.device_id();
    let attach = spawn("attach-child", move || {
        kdevice::attach_device_parent(parent_id, child_id).unwrap();
    });
    let paused = wait_for(&gate.reached);
    if !paused {
        gate.release.store(true, Ordering::SeqCst);
        let _ = attach.join();
        test_hooks::clear_pause_hook();
        assert!(paused, "the attach never reached the topology pause");
        return unittest::TestResult::Failed;
    }

    let detach = spawn("detach-child", move || {
        kdevice::detach_device_parent(child_id).unwrap();
    });
    let detach_blocked = wait_until_blocked(&detach);

    gate.release.store(true, Ordering::SeqCst);
    let attach_joined = attach.join();
    let detach_joined = detach.join();
    test_hooks::clear_pause_hook();
    assert_eq!(attach_joined, 0);
    assert_eq!(detach_joined, 0);
    assert!(
        detach_blocked.is_ok(),
        "the detach did not park on the child's relation lock: {detach_blocked:?}"
    );

    let child_device = find_device(child_id).unwrap();
    let linked = parent_device.children_snapshot().contains(&child_id);
    assert_eq!(child_device.parent() == Some(parent_id), linked);
    assert_eq!(child_device.parent(), None, "detach ran after attach");
    assert!(!linked);
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn test_opposite_direction_attach_is_serialized_by_id_order() -> unittest::TestResult {
    if kcpu_id_map::nr_cpus() < 2 {
        log::info!(
            "skipping reverse-attach handshake: it needs at least two CPUs so the second \
             holder can announce itself while the first holds the ordered relation locks"
        );
        return unittest::TestResult::Ignored;
    }

    let bus = setup();
    let a = device_desc_add(desc(&bus, 1, None)).unwrap();
    let b = device_desc_add(desc(&bus, 2, None)).unwrap();
    let a_id = a.device_id();
    let b_id = b.device_id();

    let started = Arc::new(AtomicU32::new(0));
    let arrivals = Arc::new(AtomicUsize::new(0));
    let second_acquired_during_first = Arc::new(AtomicBool::new(false));
    let second_acquired = Arc::new(AtomicBool::new(false));
    let timed_out = Arc::new(AtomicBool::new(false));
    {
        let started = started.clone();
        let arrivals = arrivals.clone();
        let during_first = second_acquired_during_first.clone();
        let acquired = second_acquired.clone();
        let timed_out = timed_out.clone();
        test_hooks::install_relations_lock_hook(Arc::new(move |_ids| {
            if arrivals.fetch_add(1, Ordering::SeqCst) == 0 {
                // First holder: wait until the opposite task is runnable and
                // has set its start flag. It cannot take the relation locks
                // while we hold them, which is exactly what we verify.
                let mut spins = 0;
                while started.load(Ordering::SeqCst) != 3 && spins < HANDSHAKE_SPINS {
                    core::hint::spin_loop();
                    spins += 1;
                }
                if started.load(Ordering::SeqCst) != 3 {
                    timed_out.store(true, Ordering::SeqCst);
                }
                for _ in 0..HANDSHAKE_SPINS {
                    if acquired.load(Ordering::SeqCst) {
                        during_first.store(true, Ordering::SeqCst);
                        break;
                    }
                    core::hint::spin_loop();
                }
            } else {
                acquired.store(true, Ordering::SeqCst);
            }
        }));
    }

    // Exactly one direction may succeed. The loser either observes the cycle
    // after acquiring the ordered relation locks, or rejects it from the
    // pre-lock snapshot; both are correct, so the result count is the
    // invariant, not whether the second holder reached the lock hook.
    let results = Arc::new(SpinNoPreempt::new(Vec::new()));
    let first = started.clone();
    let first_results = results.clone();
    let task_one = spawn("attach-b-under-a", move || {
        first.fetch_or(1, Ordering::SeqCst);
        let result = kdevice::attach_device_parent(b_id, a_id);
        first_results.lock().push(result);
    });
    let second = started.clone();
    let second_results = results.clone();
    let task_two = spawn("attach-a-under-b", move || {
        second.fetch_or(2, Ordering::SeqCst);
        let result = kdevice::attach_device_parent(a_id, b_id);
        second_results.lock().push(result);
    });
    let task_one_joined = task_one.join();
    let task_two_joined = task_two.join();
    test_hooks::clear_relations_lock_hook();
    assert_eq!(task_one_joined, 0);
    assert_eq!(task_two_joined, 0);

    let results = core::mem::take(&mut *results.lock());
    assert!(!timed_out.load(Ordering::SeqCst), "the peer never reached attach");
    assert!(
        !second_acquired_during_first.load(Ordering::SeqCst),
        "the second attach acquired the ordered relation locks while the first held them"
    );
    assert_eq!(results.len(), 2, "both attaches must report a result");
    assert_eq!(
        results.iter().filter(|result| result.is_ok()).count(),
        1,
        "exactly one opposite attach succeeds; the other loses cycle detection: {results:?}"
    );
    assert!(
        results
            .iter()
            .any(|result| matches!(result, Err(DriverError::InvalidInput))),
        "the losing attach must report a cycle as InvalidInput: {results:?}"
    );
    // A second lock acquisition after the first released is legal but not
    // required, so only its absence *during* the first hold is asserted.

    let a_device = find_device(a_id).unwrap();
    let b_device = find_device(b_id).unwrap();
    let a_parent = a_device.parent();
    let b_parent = b_device.parent();
    assert!(
        !(a_parent == Some(b_id) && b_parent == Some(a_id)),
        "the two opposite attaches must not both succeed"
    );
    assert!(
        a_parent.is_some() ^ b_parent.is_some(),
        "exactly one opposite attach succeeds; the other loses cycle detection"
    );
    assert_eq!(a_parent.is_some(), b_device.children_snapshot().contains(&a_id));
    assert_eq!(b_parent.is_some(), a_device.children_snapshot().contains(&b_id));
    unittest::TestResult::Ok
}
