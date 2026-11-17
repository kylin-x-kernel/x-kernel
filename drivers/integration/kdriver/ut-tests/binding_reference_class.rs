// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Class-handle evidence for captured binding references.
//!
//! A `ClassDevice` captures the binding instance that created it. These tests
//! drive the real probe/remove lifecycle to show that:
//!
//! - driver identity stays readable from the immutable snapshot after unbind;
//! - availability follows the captured binding, not the parent's current slot;
//! - retaining a class handle never blocks removal (only `DeviceUse` does);
//! - a class handle published during probe becomes available on activation;
//! - a probe that publishes and then fails leaves its handle unavailable;
//! - the char registry filters a class entry that is not yet available.

use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use kclass::{CharDevice, CharDeviceImpl, ClassDevice, ClassPublication};
use kdevice::{
    BusTypeId, CompatibleAliasMatcher, DeviceDesc, DeviceDescSpec, DeviceDriver, DeviceId,
    DeviceIdentity, DeviceLocation, DeviceMatcher, DeviceObject, DeviceState, DiscoveryOrigin,
    PlatformIdentity, ProbeOutcome, ResourceSet, device_desc_add, find_device, probe_device_desc,
    register_driver_object, remove_device_managed, reset_and_setup_platform_bus_for_tests,
    unregister_device_desc,
};
use kspin::SpinNoPreempt;
use unittest::{assert, assert_eq, def_test};

/// Yield iterations allowed for a peer to reach a pause. A test that never
/// reaches its handshake fails instead of hanging the whole runner.
const WAIT_YIELDS: usize = 2_000_000;

/// Wait until `flag` is set; returns `false` if the pause was never reached.
fn wait_until(flag: &AtomicBool) -> bool {
    for _ in 0..WAIT_YIELDS {
        if flag.load(Ordering::SeqCst) {
            return true;
        }
        ktask::yield_now();
    }
    false
}

#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: kclass publication requires the production \
              CharDeviceImpl boxed trait object"
)]
type ClassCell = Arc<SpinNoPreempt<Option<ClassPublication<CharDeviceImpl>>>>;

fn class_cell() -> ClassCell {
    kclass::reinstall_event_bridge_for_tests();
    Arc::new(SpinNoPreempt::new(None))
}

struct TestCharDevice {
    name: String,
}

impl Device for TestCharDevice {
    fn name(&self) -> &str {
        &self.name
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }
}

impl CharDevice for TestCharDevice {
    fn read(&self, _buf: &mut [u8]) -> DriverResult<usize> {
        Ok(0)
    }

    fn write(&self, buf: &[u8]) -> DriverResult<usize> {
        Ok(buf.len())
    }
}

#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: kclass::CharDeviceImpl is the production Box<dyn \
              CharDevice + Send + Sync> publication contract"
)]
fn test_runtime(name: &str) -> CharDeviceImpl {
    Box::new(TestCharDevice {
        name: String::from(name),
    })
}

type ProbeFn = dyn Fn(Arc<DeviceObject>) -> DriverResult<()> + Send + Sync;

struct ClassDriver {
    name: &'static str,
    matcher: CompatibleAliasMatcher,
    probe: Arc<ProbeFn>,
}

impl DeviceDriver for ClassDriver {
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
        &self.matcher
    }

    fn probe_device(&self, ctx: &kdevice::ProbeContext<'_>) -> DriverResult<()> {
        let device = ctx.device().clone();
        (self.probe)(device)
    }
}

fn class_driver(
    name: &'static str,
    alias: &'static str,
    probe: impl Fn(Arc<DeviceObject>) -> DriverResult<()> + Send + Sync + 'static,
) -> kdevice::BoxedDriver {
    kdevice::BoxedDriver::new(ClassDriver {
        name,
        matcher: CompatibleAliasMatcher(alias),
        probe: Arc::new(probe),
    })
}

fn char_desc(bus_id: kdevice::BusId, alias: &'static str, location: u16) -> DeviceDesc {
    device_desc_add(DeviceDescSpec {
        bus_id,
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

/// Retire char-class entries left at an ID that a test is about to reuse.
///
/// `reset_and_setup_platform_bus_for_tests` restarts device-ID allocation at
/// zero, but the global class registries are not part of that reset. The boot
/// stdout console, for example, still owns a live char entry for the first
/// device ID, so publication for the reused ID would be rejected as a same-ID
/// conflict before any assertion runs.
///
/// This is test-environment cleanup, not evidence about production retirement:
/// call it only while preparing a test, never between a failed candidate A and
/// its replacement B.
///
/// Returns whether the cleanup left no char entry. The caller reports failure
/// with `unittest::assert!` so the test returns a proper `TestResult`.
fn retire_stale_class_entries(desc: &DeviceDesc) -> bool {
    kclass::remove_class_device_for_tests(desc.device_id());
    kclass::char_publications_for_device(desc.device_id()).is_empty()
}

/// Removes the probed device and every class entry a test published when a
/// test returns early on a failed assertion. A panic-abort path does not
/// guarantee that `Drop` runs.
struct Cleanup {
    device: Option<DeviceId>,
    classes: Vec<DeviceId>,
}

impl Cleanup {
    fn new() -> Self {
        Self {
            device: None,
            classes: Vec::new(),
        }
    }

    fn track_device(&mut self, id: DeviceId) {
        self.device = Some(id);
    }

    fn track_class(&mut self, id: DeviceId) {
        if !self.classes.contains(&id) {
            self.classes.push(id);
        }
    }

    fn device_removed(&mut self) {
        self.device = None;
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(id) = self.device.take()
            && let Err(error) = remove_device_managed(id)
        {
            log::warn!("failed to remove binding-reference test device {id:?}: {error:?}");
        }
        for id in self.classes.drain(..) {
            kclass::remove_class_device_for_tests(id);
        }
    }
}

#[def_test(serial)]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: kclass publication requires the production \
              CharDeviceImpl boxed trait object"
)]
fn class_handle_keeps_historical_identity_after_unbind() -> unittest::TestResult {
    const ALIAS: &str = "binding-ref-class-identity";

    let (_bus_type, bus) = reset_and_setup_platform_bus_for_tests("class-ref-identity-bus");
    let cell = class_cell();
    let probe_cell = cell.clone();
    let driver = register_driver_object(class_driver(ALIAS, ALIAS, move |device| {
        let class = kclass::publish_char_for_test(device, test_runtime("identity-char"))?;
        *probe_cell.lock() = Some(class);
        Ok(())
    }));

    let desc = char_desc(bus.id(), ALIAS, 1);
    assert!(
        retire_stale_class_entries(&desc),
        "test setup must clear stale char entries for the reused device id"
    );
    let mut cleanup = Cleanup::new();
    let outcome = probe_device_desc(desc.id()).unwrap();
    let records = kdevice::device_records_snapshot();
    let id = records.first().expect("probe must publish a device").id;
    cleanup.track_device(id);
    cleanup.track_class(id);
    assert_eq!(outcome, ProbeOutcome::Activated);

    let class = cell.lock().take().expect("probe publishes a class handle");
    assert!(class.is_available());
    assert_eq!(class.driver_name(), ALIAS);
    assert_eq!(class.driver_id(), driver.id());
    assert!(
        kclass::char_publications_for_device(id)
            .iter()
            .any(|class| class.is_available())
    );

    // The handle shares no usage with the device: removal succeeds while the
    // class handle is still alive.
    remove_device_managed(id).unwrap();
    cleanup.device_removed();

    assert!(!class.is_available());
    assert_eq!(class.driver_name(), ALIAS);
    assert_eq!(class.driver_id(), driver.id());
    assert!(kclass::char_publications_for_device(id).is_empty());
    // Unbind keeps the stable registration object, now with no binding.
    let records = kdevice::device_records_snapshot();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, id);
    assert_eq!(records[0].state, DeviceState::Discovered);
    unittest::TestResult::Ok
}

#[def_test(serial)]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: kclass publication requires the production \
              CharDeviceImpl boxed trait object"
)]
fn class_handle_does_not_block_device_removal() -> unittest::TestResult {
    const ALIAS: &str = "binding-ref-active-class";

    let (_bus_type, bus) = reset_and_setup_platform_bus_for_tests("class-ref-active-bus");
    let cleanups = Arc::new(AtomicUsize::new(0));
    let cell = class_cell();
    let probe_cell = cell.clone();
    let probe_cleanups = cleanups.clone();
    let driver = register_driver_object(class_driver(ALIAS, ALIAS, move |device| {
        // Probe is a lifecycle write context: it owns the descriptor binding
        // lock, so it may attach devres and publish to the class registry.
        let binding = device
            .binding_ref_for_lifecycle_write()
            .expect("probe holds the descriptor binding lock");
        let cleanup_count = probe_cleanups.clone();
        binding.add_cleanup(&device, move || {
            cleanup_count.fetch_add(1, Ordering::SeqCst);
        })?;
        let published = kclass::publish_char_for_test(device, test_runtime("active-char"))?;
        *probe_cell.lock() = Some(published);
        Ok(())
    }));

    let desc = char_desc(bus.id(), ALIAS, 2);
    assert!(
        retire_stale_class_entries(&desc),
        "test setup must clear stale char entries for the reused device id"
    );
    let mut cleanup = Cleanup::new();
    let outcome = probe_device_desc(desc.id()).unwrap();
    let records = kdevice::device_records_snapshot();
    let id = records.first().expect("probe must publish a device").id;
    cleanup.track_device(id);
    cleanup.track_class(id);
    assert_eq!(outcome, ProbeOutcome::Activated);

    let device = find_device(id).expect("activated device stays registered");
    let class = cell.lock().take().expect("probe publishes a class handle");
    assert!(class.is_available());
    assert_eq!(class.driver_id(), driver.id());

    // Keep a BindingRef with the class handle. Probe-attached devres must
    // neither be delayed by either handle nor run a second time.
    let binding = device.binding_ref().expect("active device captures a ref");

    // The real DeviceUse, not the class handle, is what rejects teardown.
    let in_use = binding
        .try_acquire(&device)
        .expect("active device admits a use");
    assert_eq!(
        remove_device_managed(id),
        Err(DriverError::ResourceBusy)
    );
    assert_eq!(cleanups.load(Ordering::SeqCst), 0);
    assert_eq!(device.state(), DeviceState::Active);
    assert_eq!(
        kdevice::find_device(id).unwrap().state(),
        DeviceState::Active
    );
    assert!(class.is_available());

    drop(in_use);
    remove_device_managed(id).unwrap();
    cleanup.device_removed();

    assert_eq!(cleanups.load(Ordering::SeqCst), 1);
    assert!(!class.is_available());
    assert!(!binding.is_active());
    assert_eq!(class.driver_name(), ALIAS);
    assert_eq!(class.driver_id(), driver.id());

    // Releasing the retained handles after teardown must not re-run cleanup.
    drop(class);
    drop(binding);
    assert_eq!(cleanups.load(Ordering::SeqCst), 1);
    unittest::TestResult::Ok
}

#[def_test(serial)]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: kclass publication requires the production \
              CharDeviceImpl boxed trait object"
)]
fn class_built_at_bound_becomes_available_on_activation() -> unittest::TestResult {
    const ALIAS: &str = "binding-ref-bound-class";

    let (_bus_type, bus) = reset_and_setup_platform_bus_for_tests("class-ref-bound-bus");
    let cell = class_cell();
    let available_at_publish = Arc::new(AtomicBool::new(true));
    let probe_cell = cell.clone();
    let probe_available = available_at_publish.clone();
    let driver = register_driver_object(class_driver(ALIAS, ALIAS, move |device| {
        let class = kclass::publish_char_for_test(device, test_runtime("bound-char"))?;
        probe_available.store(class.is_available(), Ordering::SeqCst);
        *probe_cell.lock() = Some(class);
        Ok(())
    }));

    let desc = char_desc(bus.id(), ALIAS, 3);
    assert!(
        retire_stale_class_entries(&desc),
        "test setup must clear stale char entries for the reused device id"
    );
    let mut cleanup = Cleanup::new();
    let outcome = probe_device_desc(desc.id()).unwrap();
    let records = kdevice::device_records_snapshot();
    let id = records.first().expect("probe must publish a device").id;
    cleanup.track_device(id);
    cleanup.track_class(id);
    assert_eq!(outcome, ProbeOutcome::Activated);

    // During probe the parent is Bound; the captured binding becomes
    // available only after activation.
    assert!(!available_at_publish.load(Ordering::SeqCst));
    let class = cell.lock().take().expect("probe publishes a class handle");
    assert!(class.is_available());
    assert_eq!(class.driver_id(), driver.id());
    assert!(
        kclass::char_publications_for_device(id)
            .iter()
            .any(|class| class.is_available())
    );
    unittest::TestResult::Ok
}

#[def_test(serial)]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: kclass publication requires the production \
              CharDeviceImpl boxed trait object"
)]
fn failed_probe_leaves_its_class_handle_unavailable() -> unittest::TestResult {
    const ALIAS: &str = "binding-ref-failed-class";

    let (_bus_type, bus) = reset_and_setup_platform_bus_for_tests("class-ref-failed-bus");
    let cell = class_cell();
    let probe_cell = cell.clone();
    let driver = register_driver_object(class_driver(ALIAS, ALIAS, move |device| {
        let class = kclass::publish_char_for_test(device, test_runtime("failed-char"))?;
        *probe_cell.lock() = Some(class);
        Err(DriverError::BadState)
    }));

    let desc = char_desc(bus.id(), ALIAS, 4);
    assert!(
        retire_stale_class_entries(&desc),
        "test setup must clear stale char entries for the reused device id"
    );
    let mut cleanup = Cleanup::new();
    let outcome = probe_device_desc(desc.id()).unwrap();

    // Register whatever the probe produced before asserting anything. A
    // regression that unexpectedly activates the device would otherwise leave
    // a published device and class entry behind when the assertion fails.
    let records = kdevice::device_records_snapshot();
    if let Some(record) = records.first() {
        cleanup.track_device(record.id);
    }
    let class = cell.lock().take();
    if let Some(class) = &class {
        cleanup.track_class(class.id());
    }

    assert_eq!(outcome, ProbeOutcome::Unclaimed);
    // The failed probe leaves the stable object registered but unbound.
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, desc.device_id());
    assert_eq!(records[0].state, DeviceState::Discovered);
    let class = class.expect("probe publishes its handle before failing");
    assert!(!class.is_available());
    assert_eq!(class.driver_name(), ALIAS);
    assert_eq!(class.driver_id(), driver.id());
    assert!(kclass::char_publications_for_device(class.id()).is_empty());
    unittest::TestResult::Ok
}

#[def_test(serial)]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: kclass::CharDeviceImpl and DeferredNodeRetirement are production Box-based contracts exercised by this test"
)]
fn test_failed_binding_class_retired_before_same_id_rebind() -> unittest::TestResult {
    const ALIAS: &str = "binding-ref-rebind-class";
    let (_bus_type, bus) = reset_and_setup_platform_bus_for_tests("class-ref-rebind-bus");
    kclass::reinstall_event_bridge_for_tests();
    let failed_cell = class_cell();
    let rebound_cell = class_cell();

    let first_runs = Arc::new(AtomicUsize::new(0));
    let failed_probe = failed_cell.clone();
    let failed_runs = first_runs.clone();
    let first = register_driver_object(class_driver(ALIAS, ALIAS, move |device| {
        failed_runs.fetch_add(1, Ordering::SeqCst);
        let class = kclass::publish_char_for_test(device, test_runtime("failed-rebind-char"))?;
        *failed_probe.lock() = Some(class);
        Err(DriverError::BadState)
    }));
    let desc = char_desc(bus.id(), ALIAS, 5);
    assert!(
        retire_stale_class_entries(&desc),
        "test setup must clear stale char entries for the reused device id"
    );
    let mut cleanup = Cleanup::new();
    let outcome = probe_device_desc(desc.id()).unwrap();
    let records = kdevice::device_records_snapshot();
    let id = records.first().expect("a stable object exists").id;
    cleanup.track_device(id);
    cleanup.track_class(id);
    assert_eq!(outcome, ProbeOutcome::Unclaimed);
    assert_eq!(first_runs.load(Ordering::SeqCst), 1);
    let failed = failed_cell.lock().take().expect("A published before failing");
    assert_eq!(failed.id(), id);
    assert_eq!(failed.driver_id(), first.id());
    assert!(!failed.is_available());
    assert!(kclass::char_publications_for_device(id).is_empty());

    // Registering B synchronously scans the Pending descriptor and probes only
    // B; A is not re-evaluated. B publishes for the same stable device id.
    let second_runs = Arc::new(AtomicUsize::new(0));
    let rebound_probe = rebound_cell.clone();
    let rebound_runs = second_runs.clone();
    let second = register_driver_object(class_driver(ALIAS, ALIAS, move |device| {
        rebound_runs.fetch_add(1, Ordering::SeqCst);
        let class = kclass::publish_char_for_test(device, test_runtime("rebound-char"))?;
        *rebound_probe.lock() = Some(class);
        Ok(())
    }));
    assert_eq!(second_runs.load(Ordering::SeqCst), 1);
    assert_eq!(first_runs.load(Ordering::SeqCst), 1);
    let rebound = rebound_cell
        .lock()
        .take()
        .expect("B published during the registration scan");
    assert_eq!(rebound.id(), id);
    assert!(rebound.is_available());
    assert!(!rebound.binding_ref().same_binding(failed.binding_ref()));
    assert!(!failed.is_available());
    assert_eq!(rebound.driver_id(), second.id());
    assert_ne!(failed.driver_id(), second.id());
    assert_ne!(rebound.driver_id(), first.id());
    assert_eq!(kclass::char_publications_for_device(id)
        .into_iter()
        .find(|class| class.is_available())
        .map(|class| class.driver_id()), Some(second.id()));

    // The device is already Active: an explicit probe must skip it and must
    // not run B again.
    let outcome = probe_device_desc(desc.id()).unwrap();
    assert_eq!(outcome, ProbeOutcome::Skipped);
    assert_eq!(second_runs.load(Ordering::SeqCst), 1);
    assert_eq!(first_runs.load(Ordering::SeqCst), 1);
    assert!(rebound_cell.lock().is_none());
    assert_eq!(kclass::char_publications_for_device(id)
        .into_iter()
        .find(|class| class.is_available())
        .map(|class| class.driver_id()), Some(second.id()));
    assert!(kdevice::device_records_snapshot().iter().all(|record| record.id == id));
    unittest::TestResult::Ok
}

#[def_test(serial)]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: kclass::CharDeviceImpl and DeferredNodeRetirement are production Box-based contracts exercised by this test"
)]
fn test_failed_candidate_retires_before_next_candidate_in_same_round() -> unittest::TestResult {
    const ALIAS: &str = "binding-ref-same-round-class";
    let (_bus_type, bus) = reset_and_setup_platform_bus_for_tests("class-ref-same-round-bus");
    kclass::reinstall_event_bridge_for_tests();
    let failed_cell = class_cell();
    let rebound_cell = class_cell();

    let failed_probe = failed_cell.clone();
    let first = register_driver_object(class_driver(ALIAS, ALIAS, move |device| {
        let class = kclass::publish_char_for_test(device, test_runtime("failed-same-round-char"))?;
        *failed_probe.lock() = Some(class);
        Err(DriverError::BadState)
    }));
    let rebound_probe = rebound_cell.clone();
    let second = register_driver_object(class_driver(ALIAS, ALIAS, move |device| {
        let class = kclass::publish_char_for_test(device, test_runtime("rebound-same-round-char"))?;
        *rebound_probe.lock() = Some(class);
        Ok(())
    }));

    let desc = char_desc(bus.id(), ALIAS, 13);
    assert!(
        retire_stale_class_entries(&desc),
        "test setup must clear stale char entries for the reused device id"
    );
    let mut cleanup = Cleanup::new();
    let outcome = probe_device_desc(desc.id()).unwrap();
    let records = kdevice::device_records_snapshot();
    let id = records.first().expect("a stable object exists").id;
    cleanup.track_device(id);
    cleanup.track_class(id);

    assert_eq!(outcome, ProbeOutcome::Activated);
    let failed = failed_cell.lock().take().expect("A published before failing");
    let rebound = rebound_cell.lock().take().expect("B published after A retired");
    assert_eq!(failed.id(), id);
    assert_eq!(rebound.id(), id);
    assert_eq!(failed.driver_id(), first.id());
    assert_eq!(rebound.driver_id(), second.id());
    assert!(!failed.is_available());
    assert!(rebound.is_available());
    assert!(!rebound.binding_ref().same_binding(failed.binding_ref()));
    assert_eq!(kclass::char_publications_for_device(id)
        .into_iter()
        .find(|class| class.is_available())
        .map(|class| class.driver_id()), Some(second.id()));
    assert!(kdevice::device_records_snapshot().iter().all(|record| record.id == id));
    unittest::TestResult::Ok
}

#[def_test(serial)]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: kclass::CharDeviceImpl and DeferredNodeRetirement are production Box-based contracts exercised by this test"
)]
fn test_duplicate_and_stale_events_do_not_revoke_rebind() -> unittest::TestResult {
    const ALIAS: &str = "binding-ref-stale-events";
    let (_bus_type, bus) = reset_and_setup_platform_bus_for_tests("class-ref-stale-bus");
    kclass::reinstall_event_bridge_for_tests();
    let classes: Arc<SpinNoPreempt<Vec<ClassDevice<CharDeviceImpl>>>> =
        Arc::new(SpinNoPreempt::new(Vec::new()));
    let probe_classes = classes.clone();
    register_driver_object(class_driver(ALIAS, ALIAS, move |device| {
        let class = kclass::publish_char_for_test(device, test_runtime("stale-char"))?;
        probe_classes.lock().push(class.device());
        Ok(())
    }));
    let desc = char_desc(bus.id(), ALIAS, 6);
    assert!(
        retire_stale_class_entries(&desc),
        "test setup must clear stale char entries for the reused device id"
    );
    let mut cleanup = Cleanup::new();
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Activated);
    let id = kdevice::device_records_snapshot()[0].id;
    cleanup.track_device(id);
    cleanup.track_class(id);

    remove_device_managed(id).unwrap();
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Activated);
    let (old, current) = {
        let classes = classes.lock();
        assert_eq!(classes.len(), 2);
        (classes[0].clone(), classes[1].clone())
    };
    assert!(!old.binding_ref().same_binding(current.binding_ref()));
    assert!(!old.is_available());
    assert!(current.is_available());

    // Stale notifications for the old instance must not evict B.
    kdevice::dispatch_device_event_for_tests(kdevice::DeviceEvent::Removed { id });
    kdevice::dispatch_device_event_for_tests(kdevice::DeviceEvent::BindingRetired { id });
    assert!(current.is_available());
    let live = kclass::char_publications_for_device(id)
        .into_iter()
        .find(|class| class.is_available())
        .expect("B stays published");
    assert!(live.binding_ref().same_binding(current.binding_ref()));
    assert!(!old.is_available());
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn test_removed_event_after_pending_unregister_absent() -> unittest::TestResult {
    let (_bus_type, bus) = reset_and_setup_platform_bus_for_tests("class-ref-pending-bus");
    kclass::reinstall_event_bridge_for_tests();
    let events = Arc::new(AtomicUsize::new(0));
    for kind in [
        kdevice::DeviceEventKind::Removed,
        kdevice::DeviceEventKind::BindingRetired,
    ] {
        let events = events.clone();
        kdevice::subscribe_device_event_kind(
            kind,
            Arc::new(move |_event| {
                events.fetch_add(1, Ordering::SeqCst);
            }),
        );
    }

    let desc = char_desc(bus.id(), "binding-ref-pending", 12);
    assert!(unregister_device_desc(desc.id()).is_ok());
    assert_eq!(
        events.load(Ordering::SeqCst),
        0,
        "deleting a Pending registration must not emit a class revocation event"
    );
    assert!(find_device(desc.device_id()).is_none());
    unittest::TestResult::Ok
}

#[cfg(feature = "char")]
#[def_test(serial)]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: kclass publication requires the production \
              CharDeviceImpl boxed trait object"
)]
fn char_registry_publish_filters_unavailable_class() {
    let id = DeviceId::new(0xB1_0005);
    let parent = Arc::new(DeviceObject::new_bound_for_test(id, DeviceKind::Char));
    let mut cleanup = Cleanup::new();

    let class = kclass::publish_char_for_test(parent, test_runtime("bound-only-char"))
        .expect("a Bound parent publishes");
    cleanup.track_class(id);
    assert!(!class.is_available());
    assert!(kclass::char_publications_for_device(id).is_empty());
    assert!(
        kclass::char_devices()
            .iter()
            .all(|device| device.id() != id)
    );
}

/// Deferred node publication must complete before the class entry is
/// unpublished. The fixture is detached: only the node registration task runs,
/// so no probe is ever restarted after the binding becomes terminal.
#[def_test(serial)]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: kclass::CharDeviceImpl and DeferredNodeRetirement are production Box-based contracts exercised by this test"
)]
fn test_deferred_class_unpublish_waits_for_node_publication() -> unittest::TestResult {
    use kclass::{Event, EventType, InputDevice, InputDeviceId};

    const ID: u64 = 0xB1_1009;
    let id = DeviceId::new(ID);
    let fixture = Arc::new(DeviceObject::new_bound_for_test(id, DeviceKind::Input));
    struct ClassNodeCleanup {
        id: DeviceId,
    }
    impl Drop for ClassNodeCleanup {
        fn drop(&mut self) {
            kclass::remove_class_device_for_tests(self.id);
        }
    }
    struct TestInput;
    impl Device for TestInput {
        fn name(&self) -> &str {
            "deferred-input"
        }

        fn device_kind(&self) -> DeviceKind {
            DeviceKind::Input
        }
    }
    impl InputDevice for TestInput {
        fn device_id(&self) -> InputDeviceId {
            InputDeviceId::UNKNOWN
        }

        fn physical_location(&self) -> &str {
            "test"
        }

        fn unique_id(&self) -> &str {
            "deferred-input"
        }

        fn get_event_bits(&self, _: EventType, _: &mut [u8]) -> DriverResult<bool> {
            Ok(false)
        }

        fn read_event(&self) -> DriverResult<Event> {
            Err(DriverError::WouldBlock)
        }
    }

    let _cleanup = ClassNodeCleanup { id };
    let class = kclass::publish_input_for_test(fixture.clone(), Box::new(TestInput))
        .expect("the detached fixture publishes an input entry");
    let publication = class.publication_id();
    let binding = class.binding_ref().clone();
    let reached = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    {
        let reached = reached.clone();
        let release = release.clone();
        kdevice::test_hooks::install_pause_hook(Arc::new(move |point| {
            if let kdevice::test_hooks::BindingPause::NodePublishing { device_id } = point
                && device_id == id
            {
                reached.store(true, Ordering::SeqCst);
                while !release.load(Ordering::SeqCst) {
                    ktask::yield_now();
                }
            }
        }));
    }
    let unpublished = Arc::new(AtomicUsize::new(0));
    {
        let unpublished = unpublished.clone();
        kclass::test_hooks::install_pause_hook(Arc::new(move |point| {
            if let kclass::test_hooks::ClassPause::BeforeUnpublish { publication: paused, .. } =
                point
                && paused == publication
            {
                unpublished.fetch_add(1, Ordering::SeqCst);
            }
        }));
    }
    fixture.force_active_for_test();
    let activated = ktask::spawn_task(
        ktask::TaskInner::new_kthread(
            move || kclass::activate_input_for_test(id),
            "input-deferred-order".into(),
            0x8000,
        )
        .unwrap(),
    );
    let paused = wait_until(&reached);
    if !paused {
        release.store(true, Ordering::SeqCst);
        let _ = activated.join();
        kdevice::test_hooks::clear_pause_hook();
        kclass::test_hooks::clear_pause_hook();
        assert!(paused, "input activation never reached node publishing");
        return unittest::TestResult::Failed;
    }
    fixture.force_removed_for_test();
    kclass::remove_class_device_for_tests(id);
    let unpublished_before_release = unpublished.load(Ordering::SeqCst);
    release.store(true, Ordering::SeqCst);
    let activated_result = activated.join();
    kdevice::test_hooks::clear_pause_hook();
    kclass::test_hooks::clear_pause_hook();
    assert_eq!(activated_result, 0);

    assert!(binding.is_terminal());
    assert_eq!(
        unpublished_before_release, 0,
        "the class must not be unpublished while its node is still publishing"
    );
    assert_eq!(
        unpublished.load(Ordering::SeqCst),
        1,
        "the deferred completion unpublishes the class exactly once"
    );
    assert!(
        kdevice::device_nodes_snapshot()
            .iter()
            .all(|node| node.core_id() != id),
        "the deferred input node is gone after completion"
    );
    assert!(!class.is_available());
    unittest::TestResult::Ok
}

/// A revocation that arrives while the publication's node is already being
/// removed must return Deferred, then complete from the node callback.
#[def_test(serial)]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: InputDeviceImpl and DeferredNodeRetirement are production Box contracts"
)]
fn node_removal_deferred_callback_finishes_publication() -> unittest::TestResult {
    use kclass::{Event, EventType, InputDevice, InputDeviceId};

    struct TestInput;
    impl Device for TestInput {
        fn name(&self) -> &str {
            "node-deferred-input"
        }
        fn device_kind(&self) -> DeviceKind {
            DeviceKind::Input
        }
    }
    impl InputDevice for TestInput {
        fn device_id(&self) -> InputDeviceId {
            InputDeviceId::UNKNOWN
        }
        fn physical_location(&self) -> &str {
            "test"
        }
        fn unique_id(&self) -> &str {
            "node-deferred-input"
        }
        fn get_event_bits(&self, _: EventType, _: &mut [u8]) -> DriverResult<bool> {
            Ok(false)
        }
        fn read_event(&self) -> DriverResult<Event> {
            Err(DriverError::WouldBlock)
        }
    }

    const ID: u64 = 0xB1_1010;
    let id = DeviceId::new(ID);
    let fixture = Arc::new(DeviceObject::new_bound_for_test(id, DeviceKind::Input));
    let publication = kclass::publish_input_for_test(fixture.clone(), Box::new(TestInput))
        .expect("input publication");
    let publication_id = publication.publication_id();
    let binding = publication.binding_ref().clone();
    fixture.force_active_for_test();
    kclass::activate_input_for_test(id);
    assert!(publication.is_available());
    assert!(kdevice::device_nodes_snapshot().iter().any(|node| node.publication_id() == publication_id));

    let reached = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    {
        let reached = reached.clone();
        let release = release.clone();
        kdevice::test_hooks::install_pause_hook(Arc::new(move |point| {
            if let kdevice::test_hooks::BindingPause::NodeRemoving { device_id } = point
                && device_id == id
            {
                reached.store(true, Ordering::SeqCst);
                while !release.load(Ordering::SeqCst) {
                    ktask::yield_now();
                }
            }
        }));
    }

    let node_task = ktask::spawn_task(
        ktask::TaskInner::new_kthread(
            move || {
                let _ = kdevice::unregister_device_node_for_publication(
                    id,
                    publication_id,
                    &binding,
                    Box::new(|| {}),
                );
            },
            "node-deferred-removal".into(),
            0x8000,
        )
        .unwrap(),
    );
    assert!(wait_until(&reached), "node removal did not reach its pause");
    // The publication is Available and the node is already mid-removal, so
    // this Deferred can only come from the NodeRemoval::Deferred arm (not
    // from activation request-time deferral).
    assert!(publication.is_available(), "activation must have completed before removal");
    assert_eq!(
        publication.revoke().expect("class revoke accepted"),
        kclass::RevocationOutcome::Deferred
    );
    assert!(!publication.is_available());
    assert!(kclass::find_input_publication(publication_id).is_none());
    release.store(true, Ordering::SeqCst);
    assert_eq!(node_task.join(), 0);
    kdevice::test_hooks::clear_pause_hook();
    assert!(kclass::find_input_publication(publication_id).is_none());
    assert!(kclass::input_publications_for_device(id).is_empty());
    let _ = kdevice::remove_device_managed(id);
    unittest::TestResult::Ok
}
