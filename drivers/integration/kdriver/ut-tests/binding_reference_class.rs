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
};
use kspin::SpinNoPreempt;
use unittest::{assert, assert_eq, def_test};

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
    assert!(kdevice::device_records_snapshot().is_empty());
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
    let driver = register_driver_object(class_driver(ALIAS, ALIAS, |_device| Ok(())));

    let desc = char_desc(bus.id(), ALIAS, 2);
    let mut cleanup = Cleanup::new();
    let outcome = probe_device_desc(desc.id()).unwrap();
    let records = kdevice::device_records_snapshot();
    let id = records.first().expect("probe must publish a device").id;
    cleanup.track_device(id);
    cleanup.track_class(id);
    assert_eq!(outcome, ProbeOutcome::Activated);

    let device = find_device(id).expect("activated device stays registered");
    let class =
        kclass::publish_char(device.clone(), test_runtime("active-char")).expect("active publish");
    assert!(class.is_available());
    assert_eq!(class.driver_id(), driver.id());

    // Register a devres cleanup after activation. Retained class/BindingRef
    // handles must neither delay it nor let it run a second time.
    let cleanups = Arc::new(AtomicUsize::new(0));
    let cleanup_count = cleanups.clone();
    device.add_cleanup(move || {
        cleanup_count.fetch_add(1, Ordering::SeqCst);
    });
    let binding = device.binding_ref().expect("active device captures a ref");

    // The real DeviceUse, not the class handle, is what rejects teardown.
    let in_use = device.try_acquire().expect("active device admits a use");
    assert_eq!(remove_device_managed(id), Err(DriverError::ResourceBusy));
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
    assert!(records.is_empty());
    let class = class.expect("probe publishes its handle before failing");
    assert!(!class.is_available());
    assert_eq!(class.driver_name(), ALIAS);
    assert_eq!(class.driver_id(), driver.id());
    assert!(kclass::find_char_device(class.id()).is_none());
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
