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
use kclass::{CharDevice, CharDeviceImpl, ClassDevice};
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
type ClassCell = Arc<SpinNoPreempt<Option<ClassDevice<CharDeviceImpl>>>>;

fn class_cell() -> ClassCell {
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

fn class_driver(
    name: &'static str,
    alias: &'static str,
    probe: impl Fn(Arc<DeviceObject>) -> DriverResult<()> + Send + Sync + 'static,
) -> Arc<dyn DeviceDriver> {
    Arc::new(ClassDriver {
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
    kclass::find_char_device(desc.device_id()).is_none()
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
        let class = kclass::publish_char(device, test_runtime("identity-char"))?;
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
    assert!(kclass::find_char_device(id).is_some());

    // The handle shares no usage with the device: removal succeeds while the
    // class handle is still alive.
    remove_device_managed(id).unwrap();
    cleanup.device_removed();

    assert!(!class.is_available());
    assert_eq!(class.driver_name(), ALIAS);
    assert_eq!(class.driver_id(), driver.id());
    assert!(kclass::find_char_device(id).is_none());
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
        let published = kclass::publish_char(device, test_runtime("active-char"))?;
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
        let class = kclass::publish_char(device, test_runtime("bound-char"))?;
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
    assert!(kclass::find_char_device(id).is_some());
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
        let class = kclass::publish_char(device, test_runtime("failed-char"))?;
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
    assert!(kclass::find_char_device(class.id()).is_none());
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
        let class = kclass::publish_char(device, test_runtime("failed-rebind-char"))?;
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
    assert!(kclass::find_char_device(id).is_none());

    // Registering B synchronously scans the Pending descriptor and probes only
    // B; A is not re-evaluated. B publishes for the same stable device id.
    let second_runs = Arc::new(AtomicUsize::new(0));
    let rebound_probe = rebound_cell.clone();
    let rebound_runs = second_runs.clone();
    let second = register_driver_object(class_driver(ALIAS, ALIAS, move |device| {
        rebound_runs.fetch_add(1, Ordering::SeqCst);
        let class = kclass::publish_char(device, test_runtime("rebound-char"))?;
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
    assert_eq!(kclass::find_char_device(id).map(|class| class.driver_id()), Some(second.id()));

    // The device is already Active: an explicit probe must skip it and must
    // not run B again.
    let outcome = probe_device_desc(desc.id()).unwrap();
    assert_eq!(outcome, ProbeOutcome::Skipped);
    assert_eq!(second_runs.load(Ordering::SeqCst), 1);
    assert_eq!(first_runs.load(Ordering::SeqCst), 1);
    assert!(rebound_cell.lock().is_none());
    assert_eq!(kclass::find_char_device(id).map(|class| class.driver_id()), Some(second.id()));
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
        let class = kclass::publish_char(device, test_runtime("failed-same-round-char"))?;
        *failed_probe.lock() = Some(class);
        Err(DriverError::BadState)
    }));
    let rebound_probe = rebound_cell.clone();
    let second = register_driver_object(class_driver(ALIAS, ALIAS, move |device| {
        let class = kclass::publish_char(device, test_runtime("rebound-same-round-char"))?;
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
    assert_eq!(kclass::find_char_device(id).map(|class| class.driver_id()), Some(second.id()));
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
        let class = kclass::publish_char(device, test_runtime("stale-char"))?;
        probe_classes.lock().push(class);
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
    let live = kclass::find_char_device(id).expect("B stays published");
    assert!(live.binding_ref().same_binding(current.binding_ref()));
    assert!(!old.is_available());
    unittest::TestResult::Ok
}

fn drm_node_class() -> kdevice::DeviceNodeClass {
    kdevice::DeviceNodeClass::Drm(kdevice::DrmDeviceNodeMetadata::new(
        String::from("Virtual-1"),
        String::from("connected"),
        String::from("On"),
        String::from("1280x800\n"),
        Vec::new(),
    ))
}

#[def_test(serial)]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: DeferredNodeRetirement is a production Box<dyn FnOnce()> callback contract exercised by this test"
)]
fn test_legacy_node_api_respects_owner() -> unittest::TestResult {
    const ALIAS: &str = "binding-ref-node-owner";
    let (_bus_type, bus) = reset_and_setup_platform_bus_for_tests("class-ref-node-bus");
    register_driver_object(class_driver(ALIAS, ALIAS, |device| {
        let binding = device
            .binding_ref_for_lifecycle_write()
            .ok_or(DriverError::BadState)?;
        kdevice::register_device_node_for_binding(
            device.id(),
            &binding,
            drm_node_class(),
            String::from("node-owner"),
            String::from("node-driver"),
            DeviceLocation::PlatformStatic { id: 7 },
            None,
        )?;
        Ok(())
    }));
    let desc = char_desc(bus.id(), ALIAS, 7);
    let mut cleanup = Cleanup::new();
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Activated);
    let id = kdevice::device_records_snapshot()[0].id;
    cleanup.track_device(id);
    let device = find_device(id).unwrap();
    let binding = device.binding_ref().expect("activated device has a binding");

    assert!(kdevice::device_nodes_snapshot().iter().any(|node| node.core_id() == id));
    assert_eq!(
        kdevice::unregister_device_node(id),
        Err(DriverError::BadState),
        "the legacy id-only API must not remove another binding's node"
    );
    assert!(kdevice::device_nodes_snapshot().iter().any(|node| node.core_id() == id));

    let wrong_owner = DeviceObject::new_bound_for_test(DeviceId::new(0xB1_2001), DeviceKind::Char)
        .binding_ref()
        .expect("fixture has a binding");
    let deferred_ran = Arc::new(AtomicBool::new(false));
    let deferred_flag = deferred_ran.clone();
    assert!(matches!(
        kdevice::unregister_device_node_for_binding(id, &wrong_owner, Box::new(move || {
            deferred_flag.store(true, Ordering::SeqCst);
        })),
        kdevice::NodeRemoval::NotOwner
    ));
    assert!(!deferred_ran.load(Ordering::SeqCst));
    assert!(kdevice::device_nodes_snapshot().iter().any(|node| node.core_id() == id));

    assert_eq!(
        kdevice::register_device_node(
            id,
            drm_node_class(),
            String::from("legacy-duplicate"),
            String::from("node-driver"),
            DeviceLocation::PlatformStatic { id: 7 },
            None,
        )
        .err(),
        Some(DriverError::ResourceBusy),
        "an ownerless registration cannot take over an owned node"
    );

    assert!(matches!(
        kdevice::unregister_device_node_for_binding(id, &binding, Box::new(|| {})),
        kdevice::NodeRemoval::Removed
    ));
    assert!(kdevice::device_nodes_snapshot().iter().all(|node| node.core_id() != id));
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
    reason = "CG-049-no-std-box-in-kernel-tests: DeferredNodeRetirement is a production Box<dyn FnOnce()> callback contract exercised by this test"
)]
fn test_deferred_retirement_registered_and_consumed_once() -> unittest::TestResult {
    const ALIAS: &str = "binding-ref-node-deferred";
    let (_bus_type, bus) = reset_and_setup_platform_bus_for_tests("class-ref-node-deferred-bus");
    let binding_cell = Arc::new(SpinNoPreempt::new(None::<kdevice::BindingRef>));
    let id_cell = Arc::new(SpinNoPreempt::new(None::<DeviceId>));
    let reached = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    {
        let reached = reached.clone();
        let release = release.clone();
        kdevice::test_hooks::install_pause_hook(Arc::new(move |point| {
            if matches!(point, kdevice::test_hooks::BindingPause::NodePublishing { .. }) {
                reached.store(true, Ordering::SeqCst);
                while !release.load(Ordering::SeqCst) {
                    ktask::yield_now();
                }
            }
        }));
    }

    let probe_binding = binding_cell.clone();
    let probe_id = id_cell.clone();
    register_driver_object(class_driver(ALIAS, ALIAS, move |device| {
        let binding = device
            .binding_ref_for_lifecycle_write()
            .ok_or(DriverError::BadState)?;
        // Publish the test's handles before registration: the publisher pause
        // fires inside `register_device_node_for_binding`.
        *probe_binding.lock() = Some(binding.clone());
        *probe_id.lock() = Some(device.id());
        kdevice::register_device_node_for_binding(
            device.id(),
            &binding,
            drm_node_class(),
            String::from("deferred-node"),
            String::from("node-driver"),
            DeviceLocation::PlatformStatic { id: 8 },
            None,
        )?;
        Ok(())
    }));
    let desc = char_desc(bus.id(), ALIAS, 8);
    let mut cleanup = Cleanup::new();
    let bus_probe = spawn_probe(desc.id());

    let paused = wait_until(&reached);
    let id = id_cell.lock().take();
    let binding = binding_cell.lock().clone();
    if !paused || id.is_none() || binding.is_none() {
        release.store(true, Ordering::SeqCst);
        let _ = bus_probe.join();
        kdevice::test_hooks::clear_pause_hook();
        assert!(paused, "the probe never reached the node publisher pause");
        assert!(id.is_some(), "the probe published no device id");
        assert!(binding.is_some(), "the probe published no binding ref");
        return unittest::TestResult::Failed;
    }
    let id = id.unwrap();
    let binding = binding.unwrap();
    cleanup.track_device(id);

    // Run both removal requests while the node is still publishing; the
    // observations are asserted only after the probe is released and joined.
    let deferred_runs = Arc::new(AtomicUsize::new(0));
    let deferred = deferred_runs.clone();
    let first_removal =
        kdevice::unregister_device_node_for_binding(id, &binding, Box::new(move || {
            deferred.store(1, Ordering::SeqCst);
        }));
    let duplicate_runs = Arc::new(AtomicUsize::new(0));
    let duplicate = duplicate_runs.clone();
    let second_removal =
        kdevice::unregister_device_node_for_binding(id, &binding, Box::new(move || {
            duplicate.store(1, Ordering::SeqCst);
        }));
    let deferred_before_release = deferred_runs.load(Ordering::SeqCst);
    let duplicate_before_release = duplicate_runs.load(Ordering::SeqCst);
    let absent_before_release = kdevice::device_nodes_snapshot()
        .iter()
        .all(|node| node.core_id() != id);

    release.store(true, Ordering::SeqCst);
    let probe_joined = bus_probe.join();
    kdevice::test_hooks::clear_pause_hook();
    assert_eq!(probe_joined, 0);

    assert!(
        matches!(first_removal, kdevice::NodeRemoval::Deferred),
        "a node still publishing defers its removal"
    );
    assert!(
        matches!(second_removal, kdevice::NodeRemoval::DeferredExisting),
        "a duplicate deferred removal reports the existing completion owner"
    );
    assert_eq!(deferred_before_release, 0);
    assert_eq!(duplicate_before_release, 0);
    assert!(absent_before_release);
    assert_eq!(
        deferred_runs.load(Ordering::SeqCst),
        1,
        "the deferred retirement runs exactly once when publishing finishes"
    );
    assert_eq!(
        duplicate_runs.load(Ordering::SeqCst),
        0,
        "a duplicate request must not replace or add a second completion callback"
    );
    assert!(kdevice::device_nodes_snapshot().iter().all(|node| node.core_id() != id));

    remove_device_managed(id).unwrap();
    cleanup.device_removed();
    unittest::TestResult::Ok
}

fn spawn_probe(desc_id: kdevice::DeviceDescId) -> ktask::KtaskRef {
    ktask::spawn_task(
        ktask::TaskInner::new_kthread(
            move || {
                probe_device_desc(desc_id).unwrap();
            },
            "probe-node-deferred".into(),
            0x8000,
        )
        .unwrap(),
    )
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
fn test_stale_event_during_busy_removing_keeps_class_and_node() -> unittest::TestResult {
    const ALIAS: &str = "binding-ref-busy-removing";
    let (_bus_type, bus) = reset_and_setup_platform_bus_for_tests("class-ref-busy-bus");
    kclass::reinstall_event_bridge_for_tests();
    let cell = class_cell();
    let probe_cell = cell.clone();
    register_driver_object(class_driver(ALIAS, ALIAS, move |device| {
        let class = kclass::publish_char(device.clone(), test_runtime("busy-char"))?;
        *probe_cell.lock() = Some(class);
        let binding = device
            .binding_ref_for_lifecycle_write()
            .ok_or(DriverError::BadState)?;
        kdevice::register_device_node_for_binding(
            device.id(),
            &binding,
            drm_node_class(),
            String::from("busy-node"),
            String::from("node-driver"),
            DeviceLocation::PlatformStatic { id: 11 },
            None,
        )?;
        Ok(())
    }));
    let desc = char_desc(bus.id(), ALIAS, 11);
    assert!(
        retire_stale_class_entries(&desc),
        "test setup must clear stale char entries for the reused device id"
    );
    let mut cleanup = Cleanup::new();
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Activated);
    let id = kdevice::device_records_snapshot()[0].id;
    cleanup.track_device(id);
    cleanup.track_class(id);
    let class = cell.lock().take().expect("probe publishes a class handle");
    assert!(class.is_available());
    let device = find_device(id).expect("activated device stays registered");
    let binding = device.binding_ref().expect("activated device captures a ref");
    let in_use = binding.try_acquire(&device).expect("active device admits a use");

    let reached = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let terminal_at_pause = Arc::new(AtomicBool::new(true));
    {
        let reached = reached.clone();
        let release = release.clone();
        let terminal_at_pause = terminal_at_pause.clone();
        let binding = binding.clone();
        kdevice::test_hooks::install_pause_hook(Arc::new(move |point| {
            if matches!(point, kdevice::test_hooks::BindingPause::BindingRemovingBeforeUseCheck) {
                terminal_at_pause.store(binding.is_terminal(), Ordering::SeqCst);
                reached.store(true, Ordering::SeqCst);
                while !release.load(Ordering::SeqCst) {
                    ktask::yield_now();
                }
            }
        }));
    }

    let busy = Arc::new(AtomicBool::new(false));
    let busy_flag = busy.clone();
    let removal = ktask::spawn_task(
        ktask::TaskInner::new_kthread(
            move || {
                let result = remove_device_managed(id);
                busy_flag.store(
                    matches!(result, Err(DriverError::ResourceBusy)),
                    Ordering::SeqCst,
                );
            },
            "remove-busy".into(),
            0x8000,
        )
        .unwrap(),
    );
    let paused = wait_until(&reached);
    if !paused {
        release.store(true, Ordering::SeqCst);
        let _ = removal.join();
        kdevice::test_hooks::clear_pause_hook();
        assert!(paused, "removal never reached the binding-removal pause");
        return unittest::TestResult::Failed;
    }

    // Capture the paused window and the reaction to stale events. Assertions
    // run only after the removal task has been released and joined so a
    // failure can never leave a task parked in the pause hook.
    let terminal_while_paused = terminal_at_pause.load(Ordering::SeqCst);
    let available_while_paused = class.is_available();
    let state_while_paused = class.binding_ref().state();
    let node_while_paused = kdevice::device_nodes_snapshot()
        .iter()
        .any(|node| node.core_id() == id);

    kdevice::dispatch_device_event_for_tests(kdevice::DeviceEvent::Removed { id });
    kdevice::dispatch_device_event_for_tests(kdevice::DeviceEvent::BindingRetired { id });
    let terminal_after_stale = class.binding_ref().is_terminal();
    let state_after_stale = class.binding_ref().state();
    let node_after_stale = kdevice::device_nodes_snapshot()
        .iter()
        .any(|node| node.core_id() == id);

    release.store(true, Ordering::SeqCst);
    let removal_joined = removal.join();
    kdevice::test_hooks::clear_pause_hook();
    assert_eq!(removal_joined, 0);

    // `Removing` is not terminal, so late notifications from the previous
    // binding must not revoke this one.
    assert!(
        !terminal_while_paused,
        "Removing must not be reported as a terminal binding"
    );
    // `Removing` closes admission, so availability is temporarily false; the
    // class entry itself must still exist and become available again after
    // the busy removal backs off.
    assert!(!available_while_paused);
    assert_eq!(state_while_paused, DeviceState::Removing);
    assert!(node_while_paused);
    assert!(
        !terminal_after_stale,
        "a stale event must not retire the live binding"
    );
    assert_eq!(state_after_stale, DeviceState::Removing);
    assert!(node_after_stale);
    assert!(busy.load(Ordering::SeqCst), "the held use must roll removal back");
    assert_eq!(device.state(), DeviceState::Active);
    assert!(class.is_available());
    assert!(kclass::find_char_device(id).is_some());
    assert!(kdevice::device_nodes_snapshot().iter().any(|node| node.core_id() == id));

    drop(in_use);
    remove_device_managed(id).unwrap();
    cleanup.device_removed();
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

    let class = kclass::publish_char(parent, test_runtime("bound-only-char"))
        .expect("a Bound parent publishes");
    cleanup.track_class(id);
    assert!(!class.is_available());
    assert!(kclass::find_char_device(id).is_none());
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
    const ID: u64 = 0xB1_1009;
    let id = DeviceId::new(ID);
    let fixture = Arc::new(DeviceObject::new_bound_for_test(id, DeviceKind::Char));
    struct ClassNodeCleanup {
        id: DeviceId,
    }
    impl Drop for ClassNodeCleanup {
        fn drop(&mut self) {
            kclass::remove_class_device_for_tests(self.id);
        }
    }
    let _cleanup = ClassNodeCleanup { id };
    let class = kclass::publish_char(fixture.clone(), test_runtime("deferred-order-char"))
        .expect("the detached fixture publishes a class entry");
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
    let claimed = Arc::new(AtomicUsize::new(0));
    let unpublished = Arc::new(AtomicUsize::new(0));
    {
        let claimed = claimed.clone();
        let unpublished = unpublished.clone();
        kclass::test_hooks::install_pause_hook(Arc::new(move |point| match point {
            kclass::test_hooks::ClassPause::RetirementClaimed { id: paused } if paused == id => {
                claimed.fetch_add(1, Ordering::SeqCst);
            }
            kclass::test_hooks::ClassPause::RetirementUnpublished { id: paused } if paused == id => {
                unpublished.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }));
    }

    let result_cell = Arc::new(SpinNoPreempt::new(None::<DriverResult<()>>));
    let node_result = result_cell.clone();
    let node_binding = binding.clone();
    let node = ktask::spawn_task(
        ktask::TaskInner::new_kthread(
            move || {
                let result = kdevice::register_device_node_for_binding(
                    id,
                    &node_binding,
                    drm_node_class(),
                    String::from("deferred-order-node"),
                    String::from("node-driver"),
                    DeviceLocation::PlatformStatic { id: 9 },
                    None,
                )
                .map(|_| ());
                *node_result.lock() = Some(result);
            },
            "node-deferred-order".into(),
            0x8000,
        )
        .unwrap(),
    );

    let paused = wait_until(&reached);
    if !paused {
        release.store(true, Ordering::SeqCst);
        let _ = node.join();
        kdevice::test_hooks::clear_pause_hook();
        kclass::test_hooks::clear_pause_hook();
        assert!(paused, "the node registration never reached its publish pause");
        return unittest::TestResult::Failed;
    }

    fixture.force_removed_for_test();
    let terminal_before_release = binding.is_terminal();
    kclass::remove_class_device_for_tests(id);
    let claimed_before_release = claimed.load(Ordering::SeqCst);
    let unpublished_before_release = unpublished.load(Ordering::SeqCst);

    release.store(true, Ordering::SeqCst);
    let joined = node.join();
    kdevice::test_hooks::clear_pause_hook();
    kclass::test_hooks::clear_pause_hook();

    let node_result = result_cell
        .lock()
        .take()
        .expect("node registration recorded a result");
    assert_eq!(joined, 0);
    node_result.expect("node registration completes after its publish pause");
    assert!(
        terminal_before_release,
        "the fixture binding must be terminal before the deferred removal is requested"
    );
    assert_eq!(
        claimed_before_release, 0,
        "the class must not be claimed while its node is still publishing"
    );
    assert_eq!(
        unpublished_before_release, 0,
        "the class must not be unpublished while its node is still publishing"
    );
    assert_eq!(
        claimed.load(Ordering::SeqCst),
        1,
        "the deferred completion claims the class exactly once"
    );
    assert_eq!(
        unpublished.load(Ordering::SeqCst),
        1,
        "the deferred completion unpublishes the class exactly once"
    );
    assert!(binding.is_terminal(), "the fixture binding stays terminal");
    assert!(
        kdevice::device_nodes_snapshot()
            .iter()
            .all(|node| node.core_id() != id),
        "the deferred node is gone after completion"
    );
    unittest::TestResult::Ok
}
