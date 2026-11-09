// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cross-task descriptor binding regressions.
//!
//! These tests exercise the descriptor binding lock with real kernel tasks
//! (`ktask`) and deterministic semaphore handshakes. They live in `kdriver`
//! because `kdevice` does not depend on the task layer.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

use driver_base::{DeviceKind, DriverError, DriverResult};
use kdevice::{
    self, ActiveDeviceAdoption, BusInstance, BusTypeId, CompatibleAliasMatcher, DeviceDesc,
    DeviceDescSpec, DeviceDriver, DeviceEvent, DeviceEventKind, DeviceId, DeviceIdentity,
    DeviceLocation, DeviceMatcher, DeviceObject, DeviceRecord, DeviceState, DiscoveryOrigin,
    PlatformIdentity, ProbeOutcome, ResourceSet,
};
use kspin::SpinNoPreempt;
use ksync::Semaphore;
use unittest::def_test;

use kdevice::test_hooks::{self, BindingPause};

type ProbeFn = dyn Fn(Arc<DeviceObject>) -> DriverResult<()> + Send + Sync;

struct ThreadDriver {
    name: &'static str,
    matcher: CompatibleAliasMatcher,
    probe: Arc<ProbeFn>,
}

impl DeviceDriver for ThreadDriver {
    fn name(&self) -> &'static str {
        self.name
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn DeviceMatcher {
        &self.matcher
    }

    fn probe_device(&self, device: Arc<DeviceObject>) -> DriverResult<()> {
        (self.probe)(device)
    }
}

fn driver(
    name: &'static str,
    alias: &'static str,
    probe: impl Fn(Arc<DeviceObject>) -> DriverResult<()> + Send + Sync + 'static,
) -> Arc<dyn DeviceDriver> {
    Arc::new(ThreadDriver {
        name,
        matcher: CompatibleAliasMatcher(alias),
        probe: Arc::new(probe),
    })
}

fn setup() -> Arc<BusInstance> {
    // A test that panics before its cleanup must not leave a blocked hook for
    // the next serial test.
    test_hooks::clear_pause_hook();
    kdevice::reset_and_setup_platform_bus_for_tests("binding-thread-bus").1
}

fn desc(bus: &BusInstance, location: u16, alias: &'static str) -> DeviceDesc {
    kdevice::device_desc_add(DeviceDescSpec {
        bus_id: bus.id(),
        parent: None,
        location: DeviceLocation::PlatformStatic { id: location },
        origin: DiscoveryOrigin::PlatformStatic,
        identity: DeviceIdentity::Platform(PlatformIdentity {
            alias: Some(alias),
            firmware_id: None,
        }),
        transport: None,
        resources: ResourceSet::new(),
        dma: None,
        intx: None,
        firmware_node: None,
    })
    .unwrap()
}

/// Look up the published device for a descriptor through the registry view.
///
/// The bus snapshot is not used here: during remove the registry entry is
/// dropped before the bus association is detached, and these tests must
/// observe that intermediate state.
fn bound_record(bus: &BusInstance, desc: &DeviceDesc) -> Option<DeviceRecord> {
    kdevice::device_records_snapshot()
        .into_iter()
        .find(|record| record.bus_id == bus.id() && record.location == desc.location())
}

type Cell<T> = Arc<SpinNoPreempt<Option<T>>>;

fn cell<T>() -> Cell<T> {
    Arc::new(SpinNoPreempt::new(None))
}

fn set_cell<T>(cell: &Cell<T>, value: T) {
    *cell.lock() = Some(value);
}

fn get_cell<T: Copy>(cell: &Cell<T>) -> Option<T> {
    *cell.lock()
}

fn event_id(event: DeviceEvent) -> DeviceId {
    match event {
        DeviceEvent::Published { id }
        | DeviceEvent::Matched { id }
        | DeviceEvent::Removed { id } => id,
        DeviceEvent::Bound { id, .. } | DeviceEvent::Activated { id, .. } => id,
    }
}

type EventTrace = Arc<SpinNoPreempt<Vec<(DeviceEventKind, DeviceId)>>>;

fn subscribe_events() -> EventTrace {
    let trace: EventTrace = Arc::new(SpinNoPreempt::new(Vec::new()));
    for kind in [
        DeviceEventKind::Published,
        DeviceEventKind::Matched,
        DeviceEventKind::Bound,
        DeviceEventKind::Activated,
        DeviceEventKind::Removed,
    ] {
        let trace = trace.clone();
        kdevice::subscribe_device_event_kind(
            kind,
            Arc::new(move |event| trace.lock().push((event.kind(), event_id(event)))),
        );
    }
    trace
}

fn event_position(trace: &EventTrace, kind: DeviceEventKind, id: DeviceId) -> Option<usize> {
    trace
        .lock()
        .iter()
        .position(|(event_kind, event_id)| *event_kind == kind && *event_id == id)
}

/// Upper bound, in scheduler yields, for every handshake in this file.
///
/// A missed pause point or a broken serialization must surface as a
/// diagnostic after cleanup, never as a hung test.
const MAX_WAIT_YIELDS: usize = 10_000;

/// Bounded wait for a semaphore permit used by a test gate.
fn wait_permit(semaphore: &Semaphore) -> bool {
    for _ in 0..MAX_WAIT_YIELDS {
        if semaphore.try_acquire() {
            return true;
        }
        ktask::yield_now();
    }
    false
}

/// Bounded wait until `task` is parked by a blocking operation.
///
/// The only blocking operation a contender performs in these tests is the
/// descriptor binding lock, so a parked state is direct evidence that it
/// reached the lock and is waiting behind the gate holder.
fn wait_until_blocked(task: &ktask::KtaskRef) -> Result<(), ktask::TaskState> {
    for _ in 0..MAX_WAIT_YIELDS {
        let state = task.state();
        if state.is_blocked() {
            return Ok(());
        }
        if state == ktask::TaskState::Exited {
            return Err(state);
        }
        ktask::yield_now();
    }
    Err(task.state())
}

/// Bounded wait for `task` to exit, then reap it.
///
/// `join` is immediate once `Exited` is observed, so the bound covers the only
/// potentially blocking wait.
fn reap(task: &ktask::KtaskRef) -> Result<i32, ktask::TaskState> {
    for _ in 0..MAX_WAIT_YIELDS {
        if task.state() == ktask::TaskState::Exited {
            return Ok(task.join());
        }
        ktask::yield_now();
    }
    Err(task.state())
}

/// Negative control for the blocking detector used by the race tests.
///
/// `wait_until_blocked` must not silently succeed for a task that never enters
/// a blocking operation; otherwise every test built on it could pass without
/// exercising the intended window.
#[def_test(serial)]
fn test_blocking_detector_rejects_task_that_never_waits() {
    let task = ktask::spawn(|| {});
    let observed = wait_until_blocked(&task);
    let reaped = reap(&task);

    assert_eq!(observed, Err(ktask::TaskState::Exited));
    assert_eq!(reaped, Ok(0));
}

#[def_test(serial)]
fn test_waiter_rechecks_and_skips_after_holder_binds() {
    let bus = setup();
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let probes = Arc::new(AtomicUsize::new(0));

    let (probe_entered, probe_release, probe_count) =
        (entered.clone(), release.clone(), probes.clone());
    kdevice::register_driver_object(driver("thread-holder", "thread", move |_| {
        probe_count.fetch_add(1, Ordering::SeqCst);
        probe_entered.release();
        probe_release.acquire();
        Ok(())
    }));

    let desc = desc(&bus, 1, "thread");
    let id = desc.id();

    let holder_outcome = cell();
    let holder_slot = holder_outcome.clone();
    let holder = ktask::spawn(move || set_cell(&holder_slot, kdevice::probe_device_desc(id).unwrap()));
    let holder_started = wait_permit(&entered);
    if !holder_started {
        release.release();
        let _ = reap(&holder);
        assert!(holder_started, "holder never entered its probe");
    }

    let waiter_outcome = cell();
    let waiter_slot = waiter_outcome.clone();
    let waiter = ktask::spawn(move || set_cell(&waiter_slot, kdevice::probe_device_desc(id).unwrap()));
    let waiter_blocked = wait_until_blocked(&waiter);
    release.release();
    let holder_code = reap(&holder);
    let waiter_code = reap(&waiter);

    assert_eq!(
        waiter_blocked,
        Ok(()),
        "waiter was not parked on the binding lock: {waiter_blocked:?}"
    );
    assert_eq!(holder_code, Ok(0));
    assert_eq!(waiter_code, Ok(0));
    assert_eq!(get_cell(&holder_outcome), Some(ProbeOutcome::Activated));
    assert_eq!(get_cell(&waiter_outcome), Some(ProbeOutcome::Skipped));
    assert_eq!(probes.load(Ordering::SeqCst), 1);
}

#[def_test(serial)]
fn test_waiter_retries_when_holder_returns_pending() {
    let bus = setup();
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let holder_probes = Arc::new(AtomicUsize::new(0));

    let (probe_entered, probe_release, probe_count) =
        (entered.clone(), release.clone(), holder_probes.clone());
    kdevice::register_driver_object(driver("thread-fail", "retry", move |_| {
        probe_count.fetch_add(1, Ordering::SeqCst);
        probe_entered.release();
        probe_release.acquire();
        Err(DriverError::Io)
    }));

    let desc = desc(&bus, 2, "retry");
    let id = desc.id();

    let holder = ktask::spawn(move || {
        assert_eq!(
            kdevice::probe_device_desc(id).unwrap(),
            ProbeOutcome::Unclaimed
        );
    });
    let holder_started = wait_permit(&entered);
    if !holder_started {
        release.release();
        let _ = reap(&holder);
        assert!(holder_started, "holder never entered its probe");
    }

    // Arrival handshake: the scan reports that it is about to take this
    // descriptor's lock, so the parked state below can only be that lock.
    let scan_arrived = Arc::new(Semaphore::new(0));
    let arrived = scan_arrived.clone();
    test_hooks::install_pause_hook(Arc::new(move |point| {
        if let BindingPause::RegisterScanBeforeLock { desc_id } = point
            && desc_id == id
        {
            arrived.release();
        }
    }));

    let waiter_probes = Arc::new(AtomicUsize::new(0));
    let waiter_count = waiter_probes.clone();
    let waiter_driver = driver("thread-waiter", "retry", move |_| {
        waiter_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });
    let waiter = ktask::spawn(move || {
        kdevice::register_driver_object(waiter_driver);
    });
    let scan_arrived = wait_permit(&scan_arrived);
    let waiter_blocked = wait_until_blocked(&waiter);
    release.release();
    let holder_code = reap(&holder);
    let waiter_code = reap(&waiter);
    test_hooks::clear_pause_hook();

    assert!(scan_arrived, "registration scan never reached the lock handshake");
    assert_eq!(
        waiter_blocked,
        Ok(()),
        "registration scan was not parked on the binding lock: {waiter_blocked:?}"
    );
    assert_eq!(holder_code, Ok(0));
    assert_eq!(waiter_code, Ok(0));
    assert_eq!(holder_probes.load(Ordering::SeqCst), 1);
    assert_eq!(waiter_probes.load(Ordering::SeqCst), 1);
    assert_eq!(
        bound_record(&bus, &desc).unwrap().driver_name,
        Some("thread-waiter")
    );
}

#[def_test(serial)]
fn test_cross_thread_contender_never_gets_busy() {
    let bus = setup();
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));

    let (probe_entered, probe_release) = (entered.clone(), release.clone());
    kdevice::register_driver_object(driver("busy-holder", "busy", move |_| {
        probe_entered.release();
        probe_release.acquire();
        Ok(())
    }));

    let desc = desc(&bus, 3, "busy");
    let id = desc.id();

    let holder = ktask::spawn(move || {
        assert_eq!(
            kdevice::probe_device_desc(id).unwrap(),
            ProbeOutcome::Activated
        );
    });
    let holder_started = wait_permit(&entered);
    if !holder_started {
        release.release();
        let _ = reap(&holder);
        assert!(holder_started, "holder never entered its probe");
    }

    let contender_outcome = cell();
    let contender_slot = contender_outcome.clone();
    let contender =
        ktask::spawn(move || set_cell(&contender_slot, kdevice::probe_device_desc(id).unwrap()));
    let contender_blocked = wait_until_blocked(&contender);
    release.release();
    let holder_code = reap(&holder);
    let contender_code = reap(&contender);

    assert_eq!(
        contender_blocked,
        Ok(()),
        "cross-thread contender was not parked on the binding lock: {contender_blocked:?}"
    );
    assert_eq!(holder_code, Ok(0));
    assert_eq!(contender_code, Ok(0));
    let outcome = get_cell(&contender_outcome).unwrap();
    assert_ne!(outcome, ProbeOutcome::Busy);
    assert_eq!(outcome, ProbeOutcome::Skipped);
}

#[def_test(serial)]
fn test_waiter_binds_after_core_preparation_failure() {
    let bus = setup();
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let a_probes = Arc::new(AtomicUsize::new(0));
    let c_probes = Arc::new(AtomicUsize::new(0));

    let (probe_entered, probe_release, a_count) =
        (entered.clone(), release.clone(), a_probes.clone());
    kdevice::register_driver_object(driver("core-fail-a", "core", move |_| {
        a_count.fetch_add(1, Ordering::SeqCst);
        probe_entered.release();
        probe_release.acquire();
        Err(DriverError::Io)
    }));
    let c_count = c_probes.clone();
    kdevice::register_driver_object(driver("core-fail-c", "core", move |_| {
        c_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }));

    let desc = desc(&bus, 4, "core");
    let id = desc.id();
    let holder_outcome = cell();
    let holder_slot = holder_outcome.clone();

    // A constructs normally; C's construction is the injected core failure.
    test_hooks::fail_device_construction_after(1);
    let holder = ktask::spawn(move || {
        set_cell(&holder_slot, kdevice::probe_device_desc(id));
    });
    let holder_started = wait_permit(&entered);
    if !holder_started {
        release.release();
        let _ = reap(&holder);
        assert!(holder_started, "holder never reached the first candidate probe");
    }

    // Arrival handshake: the scan reports that it is about to take this
    // descriptor's lock, so the parked state below can only be that lock.
    let scan_arrived = Arc::new(Semaphore::new(0));
    let arrived = scan_arrived.clone();
    test_hooks::install_pause_hook(Arc::new(move |point| {
        if let BindingPause::RegisterScanBeforeLock { desc_id } = point
            && desc_id == id
        {
            arrived.release();
        }
    }));

    let waiter_probes = Arc::new(AtomicUsize::new(0));
    let waiter_count = waiter_probes.clone();
    let waiter_driver = driver("core-waiter", "core", move |_| {
        waiter_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });
    let waiter = ktask::spawn(move || {
        kdevice::register_driver_object(waiter_driver);
    });
    let scan_arrived = wait_permit(&scan_arrived);
    let waiter_blocked = wait_until_blocked(&waiter);
    release.release();
    let holder_code = reap(&holder);
    let waiter_code = reap(&waiter);
    test_hooks::clear_pause_hook();

    assert!(scan_arrived, "registration scan never reached the lock handshake");
    assert_eq!(
        waiter_blocked,
        Ok(()),
        "registration scan was not parked on the binding lock: {waiter_blocked:?}"
    );
    assert_eq!(holder_code, Ok(0));
    assert_eq!(waiter_code, Ok(0));
    assert_eq!(get_cell(&holder_outcome), Some(Err(DriverError::NoMemory)));
    assert_eq!(a_probes.load(Ordering::SeqCst), 1);
    assert_eq!(c_probes.load(Ordering::SeqCst), 0);
    assert_eq!(waiter_probes.load(Ordering::SeqCst), 1);
    assert_eq!(
        bound_record(&bus, &desc).unwrap().driver_name,
        Some("core-waiter")
    );
}

#[def_test(serial)]
fn test_delayed_scan_rechecks_after_claim() {
    let bus = setup();
    let c_probes = Arc::new(AtomicUsize::new(0));
    let c_count = c_probes.clone();
    kdevice::register_driver_object(driver("claim-c", "delayed", move |_| {
        c_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }));

    let desc = desc(&bus, 5, "delayed");
    let id = desc.id();
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let (scan_entered, scan_release) = (entered.clone(), release.clone());
    test_hooks::install_pause_hook(Arc::new(move |point| {
        if let BindingPause::RegisterScanBeforeLock { desc_id } = point
            && desc_id == id
        {
            scan_entered.release();
            scan_release.acquire();
        }
    }));

    let b_probes = Arc::new(AtomicUsize::new(0));
    let b_count = b_probes.clone();
    let b_driver = driver("delayed-b", "delayed", move |_| {
        b_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });
    let scan = ktask::spawn(move || {
        kdevice::register_driver_object(b_driver);
    });
    let scan_paused = wait_permit(&entered);
    if !scan_paused {
        release.release();
        let _ = reap(&scan);
        test_hooks::clear_pause_hook();
        assert!(
            scan_paused,
            "registration scan never reached RegisterScanBeforeLock"
        );
    }

    // The scan is paused before taking the lock; C claims and binds first.
    let claim_outcome = kdevice::probe_device_desc(id);

    release.release();
    let scan_code = reap(&scan);
    test_hooks::clear_pause_hook();

    assert_eq!(claim_outcome, Ok(ProbeOutcome::Activated));
    assert_eq!(scan_code, Ok(0));
    assert_eq!(c_probes.load(Ordering::SeqCst), 1);
    assert_eq!(b_probes.load(Ordering::SeqCst), 0);
    assert_eq!(
        bound_record(&bus, &desc).unwrap().driver_name,
        Some("claim-c")
    );
}

#[def_test(serial)]
fn test_remove_holds_binding_lock_through_unbind() {
    let bus = setup();
    let trace = subscribe_events();
    kdevice::register_driver_object(driver("remove-holder", "remove-lock", |_| Ok(())));

    let desc = desc(&bus, 6, "remove-lock");
    assert_eq!(kdevice::probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Activated);
    let old_id = bound_record(&bus, &desc).unwrap().id;

    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let (remove_entered, remove_release) = (entered.clone(), release.clone());
    let target = desc.id();
    test_hooks::install_pause_hook(Arc::new(move |point| {
        if let BindingPause::RemoveAfterRequeue { desc_id, .. } = point
            && desc_id == target
        {
            remove_entered.release();
            remove_release.acquire();
        }
    }));

    let remove_result = cell();
    let remove_slot = remove_result.clone();
    let remover = ktask::spawn(move || {
        set_cell(&remove_slot, kdevice::remove_device_managed(old_id));
    });
    let remove_paused = wait_permit(&entered);
    if !remove_paused {
        release.release();
        let _ = reap(&remover);
        test_hooks::clear_pause_hook();
        assert!(remove_paused, "remove never reached RemoveAfterRequeue");
    }

    // Descriptor is back to Pending, but the old bus association is still
    // listed and the binding lock is still held by the remover.
    let device_missing = kdevice::find_device(old_id).is_none();
    let bus_len_during_remove = bus.devices_snapshot().len();

    let binder_outcome = cell();
    let binder_slot = binder_outcome.clone();
    let binder = ktask::spawn(move || {
        set_cell(&binder_slot, kdevice::probe_device_desc(target).unwrap());
    });
    let binder_blocked = wait_until_blocked(&binder);
    let record_absent = bound_record(&bus, &desc).is_none();

    release.release();
    let remover_code = reap(&remover);
    let binder_code = reap(&binder);
    test_hooks::clear_pause_hook();

    assert_eq!(
        binder_blocked,
        Ok(()),
        "binder was not parked on the remove-held binding lock: {binder_blocked:?}"
    );
    assert!(device_missing);
    assert_eq!(bus_len_during_remove, 1);
    assert!(record_absent);
    assert_eq!(remover_code, Ok(0));
    assert_eq!(binder_code, Ok(0));
    assert_eq!(get_cell(&remove_result), Some(Ok(())));
    assert_eq!(get_cell(&binder_outcome), Some(ProbeOutcome::Activated));
    let new_id = bound_record(&bus, &desc).unwrap().id;
    assert_ne!(new_id, old_id);
    assert_eq!(bus.devices_snapshot().len(), 1);
    assert_eq!(bus.devices_snapshot()[0].id(), new_id);
    let removed_at = event_position(&trace, DeviceEventKind::Removed, old_id)
        .expect("old device must report Removed");
    let activated_at = event_position(&trace, DeviceEventKind::Activated, new_id)
        .expect("new device must report Activated");
    assert!(
        removed_at < activated_at,
        "old device must be removed before the new identity is activated"
    );
}

#[def_test(serial)]
fn test_remove_waits_for_inflight_probe() {
    let bus = setup();
    kdevice::register_driver_object(driver("inflight-a", "inflight", |_| Ok(())));

    let desc = desc(&bus, 7, "inflight");
    assert_eq!(kdevice::probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Activated);
    let device_id = bound_record(&bus, &desc).unwrap().id;

    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let (probe_entered, probe_release) = (entered.clone(), release.clone());
    let target = desc.id();
    test_hooks::install_pause_hook(Arc::new(move |point| {
        if let BindingPause::ProbeHoldingLock { desc_id } = point
            && desc_id == target
        {
            probe_entered.release();
            probe_release.acquire();
        }
    }));

    let probe_outcome = cell();
    let probe_slot = probe_outcome.clone();
    let holder = ktask::spawn(move || {
        set_cell(&probe_slot, kdevice::probe_device_desc(target).unwrap());
    });
    let probe_paused = wait_permit(&entered);
    if !probe_paused {
        release.release();
        let _ = reap(&holder);
        test_hooks::clear_pause_hook();
        assert!(probe_paused, "probe never reached ProbeHoldingLock");
    }

    let remove_result = cell();
    let remove_slot = remove_result.clone();
    let remover = ktask::spawn(move || {
        set_cell(&remove_slot, kdevice::remove_device_managed(device_id));
    });
    let remover_blocked = wait_until_blocked(&remover);
    // The remover must not have committed while the probe holds the lock.
    let observed_state = kdevice::find_device(device_id).map(|device| device.state());

    release.release();
    let holder_code = reap(&holder);
    let remover_code = reap(&remover);
    test_hooks::clear_pause_hook();

    assert_eq!(
        remover_blocked,
        Ok(()),
        "remover was not parked on the probe-held binding lock: {remover_blocked:?}"
    );
    assert_eq!(observed_state, Some(DeviceState::Active));
    assert_eq!(holder_code, Ok(0));
    assert_eq!(remover_code, Ok(0));
    assert_eq!(get_cell(&probe_outcome), Some(ProbeOutcome::Skipped));
    assert_eq!(get_cell(&remove_result), Some(Ok(())));
    assert!(kdevice::find_device(device_id).is_none());
    assert!(bus.devices_snapshot().is_empty());
}

#[def_test(serial)]
fn test_remove_waits_for_adoption_publish_window() {
    let bus = setup();
    let trace = subscribe_events();
    let driver = kdevice::register_driver_object(driver("adopt-a", "adopted", |_| Ok(())));

    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let (adopt_entered, adopt_release) = (entered.clone(), release.clone());
    test_hooks::install_pause_hook(Arc::new(move |point| {
        if matches!(point, BindingPause::AdoptionBeforeActivate { .. }) {
            adopt_entered.release();
            adopt_release.acquire();
        }
    }));

    let bus_id = bus.id();
    let adopted_id = cell();
    let adopted_slot = adopted_id.clone();
    let adoption = ktask::spawn(move || {
        let device = kdevice::adopt_active_device(ActiveDeviceAdoption {
            bus_id,
            parent: None,
            location: DeviceLocation::PlatformStatic { id: 8 },
            origin: DiscoveryOrigin::PlatformStatic,
            identity: DeviceIdentity::Platform(PlatformIdentity {
                alias: Some("adopted"),
                firmware_id: None,
            }),
            transport: None,
            resources: ResourceSet::new(),
            dma: None,
            driver,
        })
        .unwrap();
        set_cell(&adopted_slot, device.id());
    });
    let adopt_paused = wait_permit(&entered);
    if !adopt_paused {
        release.release();
        let _ = reap(&adoption);
        test_hooks::clear_pause_hook();
        assert!(adopt_paused, "adoption never reached AdoptionBeforeActivate");
    }

    let records = kdevice::device_records_snapshot();
    if records.len() != 1 {
        release.release();
        let _ = reap(&adoption);
        test_hooks::clear_pause_hook();
        assert_eq!(records.len(), 1, "adoption did not publish exactly one record");
    }
    let device_id = records[0].id;
    let state_during_window = records[0].state;

    let remove_result = cell();
    let remove_slot = remove_result.clone();
    let remover = ktask::spawn(move || {
        set_cell(&remove_slot, kdevice::remove_device_managed(device_id));
    });
    let remover_blocked = wait_until_blocked(&remover);
    let observed_state = kdevice::find_device(device_id).map(|device| device.state());

    release.release();
    let adoption_code = reap(&adoption);
    let remover_code = reap(&remover);
    test_hooks::clear_pause_hook();

    assert_eq!(
        remover_blocked,
        Ok(()),
        "remover was not parked on the adoption-held binding lock: {remover_blocked:?}"
    );
    assert_eq!(state_during_window, DeviceState::Bound);
    assert_eq!(observed_state, Some(DeviceState::Bound));
    assert_eq!(adoption_code, Ok(0));
    assert_eq!(remover_code, Ok(0));
    assert_eq!(get_cell(&adopted_id), Some(device_id));
    assert_eq!(get_cell(&remove_result), Some(Ok(())));
    assert!(kdevice::find_device(device_id).is_none());
    let activated_at = event_position(&trace, DeviceEventKind::Activated, device_id)
        .expect("adopted device must report Activated");
    let removed_at = event_position(&trace, DeviceEventKind::Removed, device_id)
        .expect("adopted device must report Removed");
    assert!(
        activated_at < removed_at,
        "adoption must activate before the blocked remove commits"
    );
}

#[def_test(serial)]
fn test_bus_driver_listing_survives_shared_removal_race() {
    let bus = setup();
    let driver = kdevice::register_driver_object(driver("shared-race", "shared-race", |_| Ok(())));

    let first = desc(&bus, 9, "shared-race");
    let second = desc(&bus, 10, "shared-race");
    assert_eq!(kdevice::probe_device_desc(first.id()).unwrap(), ProbeOutcome::Activated);
    let first_id = bound_record(&bus, &first).unwrap().id;
    let driver_id = driver.id();

    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let (remove_entered, remove_release) = (entered.clone(), release.clone());
    test_hooks::install_pause_hook(Arc::new(move |point| {
        if let BindingPause::RemoveAfterRequeue { device_id, .. } = point
            && device_id == first_id
        {
            remove_entered.release();
            remove_release.acquire();
        }
    }));

    let remover = ktask::spawn(move || {
        assert_eq!(kdevice::remove_device_managed(first_id), Ok(()));
    });
    let remove_paused = wait_permit(&entered);
    if !remove_paused {
        release.release();
        let _ = reap(&remover);
        test_hooks::clear_pause_hook();
        assert!(remove_paused, "remove never reached RemoveAfterRequeue");
    }

    // The remover has removed the registry entry but has not detached the
    // bus association yet. Publish the second device in that window.
    let second_outcome = kdevice::probe_device_desc(second.id());
    let second_id = bound_record(&bus, &second).map(|record| record.id);

    release.release();
    let remover_code = reap(&remover);
    test_hooks::clear_pause_hook();

    assert_eq!(second_outcome, Ok(ProbeOutcome::Activated));
    assert_eq!(remover_code, Ok(0));
    let second_id = second_id.expect("second device must be published");

    let drivers = bus.drivers_snapshot();
    assert_eq!(drivers.len(), 1);
    assert_eq!(drivers[0].id(), driver_id);
    let devices = bus.devices_snapshot();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].id(), second_id);

    assert_eq!(kdevice::remove_device_managed(second_id), Ok(()));
    assert!(bus.drivers_snapshot().is_empty());
    assert!(bus.devices_snapshot().is_empty());
}
