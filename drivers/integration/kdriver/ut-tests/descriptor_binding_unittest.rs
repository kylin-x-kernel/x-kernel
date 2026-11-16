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
use kalloc_types::KBox;
use kdevice::{
    self, ActiveDeviceAdoption, BusInstance, BusTypeId, CompatibleAliasMatcher, DeviceDesc,
    DeviceDescSpec, DeviceDriver, DeviceEvent, DeviceEventKind, DeviceId, DeviceIdentity,
    DeviceLocation, DeviceMatcher, DeviceObject, DeviceRecord, DeviceState, DiscoveryOrigin,
    MatchResult, PlatformIdentity, ProbeOutcome, ResourceSet,
};
use kspin::SpinNoPreempt;
use ksync::Semaphore;
use unittest::def_test;

use kdevice::test_hooks::{self, BindingPause};

type ProbeFn = dyn Fn(Arc<DeviceObject>) -> DriverResult<()> + Send + Sync;

struct ThreadDriver {
    name: &'static str,
    matcher: KBox<dyn DeviceMatcher>,
    probe: Arc<ProbeFn>,
}

impl DeviceDriver for ThreadDriver {
    type Data = ();
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
        self.matcher.as_ref()
    }

    fn probe_device(&self, ctx: &kdevice::ProbeContext<'_>) -> DriverResult<()> {
        let device = ctx.device().clone();
        (self.probe)(device)
    }
}

fn driver(
    name: &'static str,
    alias: &'static str,
    probe: impl Fn(Arc<DeviceObject>) -> DriverResult<()> + Send + Sync + 'static,
) -> kdevice::BoxedDriver {
    driver_with_matcher(name, CompatibleAliasMatcher(alias), probe)
}

fn driver_with_matcher(
    name: &'static str,
    matcher: impl DeviceMatcher,
    probe: impl Fn(Arc<DeviceObject>) -> DriverResult<()> + Send + Sync + 'static,
) -> kdevice::BoxedDriver {
    let matcher: KBox<dyn DeviceMatcher> =
        KBox::new(matcher).expect("test driver matcher allocation failed");
    kdevice::BoxedDriver::new_adoptable(ThreadDriver {
        name,
        matcher,
        probe: Arc::new(probe),
    })
}

/// Alias matcher that counts evaluations, so a test can prove a stale
/// descriptor snapshot was skipped before matching (not merely before probe).
struct CountingAliasMatcher {
    alias: &'static str,
    calls: Arc<AtomicUsize>,
}

impl DeviceMatcher for CountingAliasMatcher {
    fn matches(&self, desc: &DeviceDesc) -> MatchResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        CompatibleAliasMatcher(self.alias).matches(desc)
    }
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
        | DeviceEvent::BindingRetired { id }
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

fn last_event_position(trace: &EventTrace, kind: DeviceEventKind, id: DeviceId) -> Option<usize> {
    trace
        .lock()
        .iter()
        .rposition(|(event_kind, event_id)| *event_kind == kind && *event_id == id)
}

fn event_kinds(trace: &EventTrace, id: DeviceId) -> Vec<DeviceEventKind> {
    trace
        .lock()
        .iter()
        .filter(|(_, event_id)| *event_id == id)
        .map(|(kind, _)| *kind)
        .collect()
}

fn removed_count(trace: &EventTrace, id: DeviceId) -> usize {
    event_kinds(trace, id)
        .into_iter()
        .filter(|kind| *kind == DeviceEventKind::Removed)
        .count()
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

    // A's candidate setup succeeds; C's is the injected core failure.
    test_hooks::fail_candidate_setup_after(1);
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

    // The descriptor is back to Pending, but the stable object is still
    // registered and still listed on its bus; only the binding slot is going
    // to be cleared after this pause. The binding lock is still held by the
    // remover.
    let device_present = kdevice::find_device(old_id).is_some();
    let state_during_remove = kdevice::find_device(old_id).map(|device| device.state());
    let bus_len_during_remove = bus.devices_snapshot().len();

    let binder_outcome = cell();
    let binder_slot = binder_outcome.clone();
    let binder = ktask::spawn(move || {
        set_cell(&binder_slot, kdevice::probe_device_desc(target).unwrap());
    });
    let binder_blocked = wait_until_blocked(&binder);
    let record_still_stable = bound_record(&bus, &desc).map(|record| record.id);

    release.release();
    let remover_code = reap(&remover);
    let binder_code = reap(&binder);
    test_hooks::clear_pause_hook();

    assert_eq!(
        binder_blocked,
        Ok(()),
        "binder was not parked on the remove-held binding lock: {binder_blocked:?}"
    );
    assert!(device_present);
    // Removal is committed (`begin_removing`), so the current binding still in
    // the slot projects `Removing` until the requeue completes.
    assert_eq!(state_during_remove, Some(DeviceState::Removing));
    assert_eq!(bus_len_during_remove, 1);
    assert_eq!(record_still_stable, Some(old_id));
    assert_eq!(remover_code, Ok(0));
    assert_eq!(binder_code, Ok(0));
    assert_eq!(get_cell(&remove_result), Some(Ok(())));
    assert_eq!(get_cell(&binder_outcome), Some(ProbeOutcome::Activated));
    let new_id = bound_record(&bus, &desc).unwrap().id;
    // Rebind reuses the same stable object and identity.
    assert_eq!(new_id, old_id);
    assert_eq!(bus.devices_snapshot().len(), 1);
    assert_eq!(bus.devices_snapshot()[0].id(), new_id);
    let removed_at = last_event_position(&trace, DeviceEventKind::Removed, old_id)
        .expect("unbind must report Removed");
    let activated_at = last_event_position(&trace, DeviceEventKind::Activated, old_id)
        .expect("rebind must report Activated");
    assert!(
        removed_at < activated_at,
        "the replacement binding must activate only after the old binding retired"
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
    let retained = kdevice::find_device(device_id).expect("unbind keeps the stable object");
    assert_eq!(retained.state(), DeviceState::Discovered);
    assert!(retained.binding_ref().is_none());
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
    let retained = kdevice::find_device(device_id).expect("unbind keeps the adopted object");
    assert_eq!(retained.state(), DeviceState::Discovered);
    assert!(retained.binding_ref().is_none());
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

#[def_test(serial)]
fn test_probe_that_waited_on_deleted_descriptor_skips() {
    let bus = setup();
    let desc = desc(&bus, 70, "waited-delete");
    let id = desc.id();

    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let (unreg_entered, unreg_release) = (entered.clone(), release.clone());
    test_hooks::install_pause_hook(Arc::new(move |point| {
        if let BindingPause::UnregisterHoldingLock { desc_id } = point
            && desc_id == id
        {
            unreg_entered.release();
            unreg_release.acquire();
        }
    }));

    let unregister_result = cell();
    let unregister_slot = unregister_result.clone();
    let unregister =
        ktask::spawn(move || set_cell(&unregister_slot, kdevice::unregister_device_desc(id)));
    let unregister_paused = wait_permit(&entered);
    if !unregister_paused {
        release.release();
        let _ = reap(&unregister);
        test_hooks::clear_pause_hook();
        assert!(
            unregister_paused,
            "unregister never reached UnregisterHoldingLock"
        );
    }

    // The probe obtains the old descriptor's lock handle before the deletion
    // and is parked behind the unregister holder.
    let probe_outcome = cell();
    let probe_slot = probe_outcome.clone();
    let probe = ktask::spawn(move || {
        set_cell(&probe_slot, kdevice::probe_device_desc(id));
    });
    let probe_blocked = wait_until_blocked(&probe);
    release.release();
    let unregister_code = reap(&unregister);
    let probe_code = reap(&probe);
    test_hooks::clear_pause_hook();

    assert_eq!(
        probe_blocked,
        Ok(()),
        "probe was not parked on the unregister-held binding lock: {probe_blocked:?}"
    );
    assert_eq!(unregister_code, Ok(0));
    assert_eq!(probe_code, Ok(0));
    assert_eq!(get_cell(&unregister_result), Some(Ok(())));
    assert_eq!(get_cell(&probe_outcome), Some(Ok(ProbeOutcome::Skipped)));
    assert!(kdevice::find_device_desc(id).is_none());
}

#[def_test(serial)]
fn test_unregister_waits_for_inflight_probe_success() {
    let bus = setup();
    let trace = subscribe_events();
    let probes = Arc::new(AtomicUsize::new(0));
    let count = probes.clone();
    kdevice::register_driver_object(driver("unreg-probe-ok", "unreg-probe-ok", move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }));

    let desc = desc(&bus, 71, "unreg-probe-ok");
    let id = desc.id();
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let (probe_entered, probe_release) = (entered.clone(), release.clone());
    test_hooks::install_pause_hook(Arc::new(move |point| {
        if let BindingPause::ProbeHoldingLock { desc_id } = point
            && desc_id == id
        {
            probe_entered.release();
            probe_release.acquire();
        }
    }));

    let probe_outcome = cell();
    let probe_slot = probe_outcome.clone();
    let probe = ktask::spawn(move || {
        set_cell(&probe_slot, kdevice::probe_device_desc(id).unwrap());
    });
    let probe_paused = wait_permit(&entered);
    if !probe_paused {
        release.release();
        let _ = reap(&probe);
        test_hooks::clear_pause_hook();
        assert!(probe_paused, "probe never reached ProbeHoldingLock");
    }

    let unregister_result = cell();
    let unregister_slot = unregister_result.clone();
    let unregister =
        ktask::spawn(move || set_cell(&unregister_slot, kdevice::unregister_device_desc(id)));
    let unregister_blocked = wait_until_blocked(&unregister);

    release.release();
    let probe_code = reap(&probe);
    let unregister_code = reap(&unregister);
    test_hooks::clear_pause_hook();

    assert_eq!(
        unregister_blocked,
        Ok(()),
        "unregister was not parked on the probe-held binding lock: {unregister_blocked:?}"
    );
    assert_eq!(probe_code, Ok(0));
    assert_eq!(unregister_code, Ok(0));
    assert_eq!(get_cell(&probe_outcome), Some(ProbeOutcome::Activated));
    assert_eq!(get_cell(&unregister_result), Some(Ok(())));
    assert_eq!(probes.load(Ordering::SeqCst), 1);

    let records = kdevice::device_records_snapshot();
    assert!(records.is_empty());
    assert!(kdevice::find_device_desc(id).is_none());
    let device_id = trace
        .lock()
        .iter()
        .find(|(kind, _)| *kind == DeviceEventKind::Activated)
        .map(|(_, device_id)| *device_id)
        .expect("probe must activate the device before unregister removes it");
    assert_eq!(
        event_kinds(&trace, device_id),
        alloc::vec![
            DeviceEventKind::Matched,
            DeviceEventKind::Bound,
            DeviceEventKind::Published,
            DeviceEventKind::Activated,
            DeviceEventKind::Removed,
        ]
    );
    assert_eq!(removed_count(&trace, device_id), 1);
}

#[def_test(serial)]
fn test_unregister_waits_for_inflight_probe_failure() {
    let bus = setup();
    let trace = subscribe_events();
    let probes = Arc::new(AtomicUsize::new(0));
    let count = probes.clone();
    kdevice::register_driver_object(driver("unreg-probe-fail", "unreg-probe-fail", move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        Err(DriverError::Io)
    }));

    let desc = desc(&bus, 72, "unreg-probe-fail");
    let id = desc.id();
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let (probe_entered, probe_release) = (entered.clone(), release.clone());
    test_hooks::install_pause_hook(Arc::new(move |point| {
        if let BindingPause::ProbeHoldingLock { desc_id } = point
            && desc_id == id
        {
            probe_entered.release();
            probe_release.acquire();
        }
    }));

    let probe_outcome = cell();
    let probe_slot = probe_outcome.clone();
    let probe = ktask::spawn(move || {
        set_cell(&probe_slot, kdevice::probe_device_desc(id).unwrap());
    });
    let probe_paused = wait_permit(&entered);
    if !probe_paused {
        release.release();
        let _ = reap(&probe);
        test_hooks::clear_pause_hook();
        assert!(probe_paused, "probe never reached ProbeHoldingLock");
    }

    let unregister_result = cell();
    let unregister_slot = unregister_result.clone();
    let unregister =
        ktask::spawn(move || set_cell(&unregister_slot, kdevice::unregister_device_desc(id)));
    let unregister_blocked = wait_until_blocked(&unregister);

    release.release();
    let probe_code = reap(&probe);
    let unregister_code = reap(&unregister);
    test_hooks::clear_pause_hook();

    assert_eq!(
        unregister_blocked,
        Ok(()),
        "unregister was not parked on the probe-held binding lock: {unregister_blocked:?}"
    );
    assert_eq!(probe_code, Ok(0));
    assert_eq!(unregister_code, Ok(0));
    assert_eq!(get_cell(&probe_outcome), Some(ProbeOutcome::Unclaimed));
    assert_eq!(get_cell(&unregister_result), Some(Ok(())));
    assert_eq!(probes.load(Ordering::SeqCst), 1);

    assert!(kdevice::device_records_snapshot().is_empty());
    assert!(kdevice::find_device_desc(id).is_none());
    assert!(
        trace.lock().is_empty(),
        "failed probe and unregister must not emit lifecycle events"
    );
}

#[def_test(serial)]
fn test_registration_scan_skips_after_unregister() {
    let bus = setup();
    let desc = desc(&bus, 73, "scan-unreg");
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

    let probes = Arc::new(AtomicUsize::new(0));
    let count = probes.clone();
    let matcher_calls = Arc::new(AtomicUsize::new(0));
    let scan_driver = driver_with_matcher(
        "scan-after-unreg",
        CountingAliasMatcher {
            alias: "scan-unreg",
            calls: matcher_calls.clone(),
        },
        move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    );
    let scan = ktask::spawn(move || {
        kdevice::register_driver_object(scan_driver);
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

    // The scan is paused before taking the lock; unregister deletes the
    // registration, so the scan must not match or probe the stale descriptor.
    let unregister_result = kdevice::unregister_device_desc(id);
    release.release();
    let scan_code = reap(&scan);
    test_hooks::clear_pause_hook();

    assert_eq!(unregister_result, Ok(()));
    assert_eq!(scan_code, Ok(0));
    assert_eq!(probes.load(Ordering::SeqCst), 0);
    assert_eq!(matcher_calls.load(Ordering::SeqCst), 0);
    assert!(kdevice::device_records_snapshot().is_empty());
    assert!(kdevice::find_device_desc(id).is_none());
}

#[def_test(serial)]
fn test_unbind_then_unregister_tears_down_once() {
    let bus = setup();
    let trace = subscribe_events();
    let cleanups = Arc::new(AtomicUsize::new(0));
    let cleanup_count = cleanups.clone();
    kdevice::register_driver_object(driver("unbind-first", "unbind-first", move |device| {
        let cleanup_count = cleanup_count.clone();
        let binding = device
            .binding_ref_for_lifecycle_write()
            .expect("probe holds the binding write context");
        binding
            .add_cleanup(&device, move || {
                cleanup_count.fetch_add(1, Ordering::SeqCst);
            })
            .expect("probe may register cleanup");
        Ok(())
    }));

    let desc = desc(&bus, 74, "unbind-first");
    let id = desc.id();
    assert_eq!(
        kdevice::probe_device_desc(id).unwrap(),
        ProbeOutcome::Activated
    );
    let device_id = bound_record(&bus, &desc).unwrap().id;

    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let (remove_entered, remove_release) = (entered.clone(), release.clone());
    let target_device = device_id;
    test_hooks::install_pause_hook(Arc::new(move |point| {
        if let BindingPause::RemoveAfterRequeue {
            device_id: paused_id,
            ..
        } = point
            && paused_id == target_device
        {
            remove_entered.release();
            remove_release.acquire();
        }
    }));

    let remove_result = cell();
    let remove_slot = remove_result.clone();
    let remover = ktask::spawn(move || {
        set_cell(&remove_slot, kdevice::remove_device_managed(device_id));
    });
    let remove_paused = wait_permit(&entered);
    if !remove_paused {
        release.release();
        let _ = reap(&remover);
        test_hooks::clear_pause_hook();
        assert!(remove_paused, "remove never reached RemoveAfterRequeue");
    }

    let unregister_result = cell();
    let unregister_slot = unregister_result.clone();
    let unregister =
        ktask::spawn(move || set_cell(&unregister_slot, kdevice::unregister_device_desc(id)));
    let unregister_blocked = wait_until_blocked(&unregister);

    release.release();
    let remover_code = reap(&remover);
    let unregister_code = reap(&unregister);
    test_hooks::clear_pause_hook();

    assert_eq!(
        unregister_blocked,
        Ok(()),
        "unregister was not parked on the remove-held binding lock: {unregister_blocked:?}"
    );
    assert_eq!(remover_code, Ok(0));
    assert_eq!(unregister_code, Ok(0));
    assert_eq!(get_cell(&remove_result), Some(Ok(())));
    assert_eq!(get_cell(&unregister_result), Some(Ok(())));
    assert_eq!(cleanups.load(Ordering::SeqCst), 1);
    assert_eq!(removed_count(&trace, device_id), 1);
    assert!(kdevice::find_device(device_id).is_none());
    assert!(kdevice::find_device_desc(id).is_none());
}

#[def_test(serial)]
fn test_unregister_then_unbind_returns_busy_without_second_teardown() {
    let bus = setup();
    let trace = subscribe_events();
    let cleanups = Arc::new(AtomicUsize::new(0));
    let cleanup_count = cleanups.clone();
    kdevice::register_driver_object(driver("unreg-first", "unreg-first", move |device| {
        let cleanup_count = cleanup_count.clone();
        let binding = device
            .binding_ref_for_lifecycle_write()
            .expect("probe holds the binding write context");
        binding
            .add_cleanup(&device, move || {
                cleanup_count.fetch_add(1, Ordering::SeqCst);
            })
            .expect("probe may register cleanup");
        Ok(())
    }));

    let desc = desc(&bus, 75, "unreg-first");
    let id = desc.id();
    assert_eq!(
        kdevice::probe_device_desc(id).unwrap(),
        ProbeOutcome::Activated
    );
    let device_id = bound_record(&bus, &desc).unwrap().id;

    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let (unreg_entered, unreg_release) = (entered.clone(), release.clone());
    test_hooks::install_pause_hook(Arc::new(move |point| {
        if let BindingPause::UnregisterHoldingLock { desc_id } = point
            && desc_id == id
        {
            unreg_entered.release();
            unreg_release.acquire();
        }
    }));

    let unregister_result = cell();
    let unregister_slot = unregister_result.clone();
    let unregister =
        ktask::spawn(move || set_cell(&unregister_slot, kdevice::unregister_device_desc(id)));
    let unregister_paused = wait_permit(&entered);
    if !unregister_paused {
        release.release();
        let _ = reap(&unregister);
        test_hooks::clear_pause_hook();
        assert!(
            unregister_paused,
            "unregister never reached UnregisterHoldingLock"
        );
    }

    // The unbind path looks up the still-present device/descriptor, takes the
    // same binding lock, and is parked behind the unregister holder.
    let remove_result = cell();
    let remove_slot = remove_result.clone();
    let remover = ktask::spawn(move || {
        set_cell(&remove_slot, kdevice::remove_device_managed(device_id));
    });
    let remover_blocked = wait_until_blocked(&remover);

    release.release();
    let unregister_code = reap(&unregister);
    let remover_code = reap(&remover);
    test_hooks::clear_pause_hook();

    assert_eq!(
        remover_blocked,
        Ok(()),
        "unbind was not parked on the unregister-held binding lock: {remover_blocked:?}"
    );
    assert_eq!(unregister_code, Ok(0));
    assert_eq!(remover_code, Ok(0));
    assert_eq!(get_cell(&unregister_result), Some(Ok(())));
    assert_eq!(
        get_cell(&remove_result),
        Some(Err(DriverError::ResourceBusy))
    );
    assert_eq!(cleanups.load(Ordering::SeqCst), 1);
    assert_eq!(removed_count(&trace, device_id), 1);
    assert!(kdevice::find_device(device_id).is_none());
    assert!(kdevice::find_device_desc(id).is_none());
}

#[def_test(serial)]
fn test_double_unregister_tears_down_once() {
    let bus = setup();
    let trace = subscribe_events();
    let cleanups = Arc::new(AtomicUsize::new(0));
    let cleanup_count = cleanups.clone();
    kdevice::register_driver_object(driver("double-unreg", "double-unreg", move |device| {
        let cleanup_count = cleanup_count.clone();
        let binding = device
            .binding_ref_for_lifecycle_write()
            .expect("probe holds the binding write context");
        binding
            .add_cleanup(&device, move || {
                cleanup_count.fetch_add(1, Ordering::SeqCst);
            })
            .expect("probe may register cleanup");
        Ok(())
    }));

    let desc = desc(&bus, 76, "double-unreg");
    let id = desc.id();
    assert_eq!(
        kdevice::probe_device_desc(id).unwrap(),
        ProbeOutcome::Activated
    );
    let device_id = bound_record(&bus, &desc).unwrap().id;

    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let pause_hits = Arc::new(AtomicUsize::new(0));
    let (first_entered, first_release, hits) =
        (entered.clone(), release.clone(), pause_hits.clone());
    test_hooks::install_pause_hook(Arc::new(move |point| {
        // Pause only the first unregister. The second one must be proven to
        // wait on the binding lock, not on this hook; after the first resumes
        // it enters the same point and must pass straight through.
        if let BindingPause::UnregisterHoldingLock { desc_id } = point
            && desc_id == id
            && hits.fetch_add(1, Ordering::SeqCst) == 0
        {
            first_entered.release();
            first_release.acquire();
        }
    }));

    let first_result = cell();
    let first_slot = first_result.clone();
    let first = ktask::spawn(move || {
        set_cell(&first_slot, kdevice::unregister_device_desc(id));
    });
    let first_paused = wait_permit(&entered);
    if !first_paused {
        release.release();
        let _ = reap(&first);
        test_hooks::clear_pause_hook();
        assert!(first_paused, "first unregister never reached UnregisterHoldingLock");
    }

    // Proof that the second call acquired the handle before the deletion: it
    // is parked on the binding lock held by the paused first call.
    let second_result = cell();
    let second_slot = second_result.clone();
    let second = ktask::spawn(move || {
        set_cell(&second_slot, kdevice::unregister_device_desc(id));
    });
    let second_blocked = wait_until_blocked(&second);

    release.release();
    let first_code = reap(&first);
    let second_code = reap(&second);
    test_hooks::clear_pause_hook();

    assert_eq!(
        second_blocked,
        Ok(()),
        "second unregister did not wait on the binding lock: {second_blocked:?}"
    );
    assert_eq!(first_code, Ok(0));
    assert_eq!(second_code, Ok(0));
    assert_eq!(get_cell(&first_result), Some(Ok(())));
    assert_eq!(get_cell(&second_result), Some(Ok(())));
    assert_eq!(cleanups.load(Ordering::SeqCst), 1);
    assert_eq!(removed_count(&trace, device_id), 1);
    assert!(kdevice::find_device(device_id).is_none());
    assert!(kdevice::find_device_desc(id).is_none());
}
