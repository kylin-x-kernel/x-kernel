// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use driver_base::{DeviceKind, DriverError, DriverResult};
use fwnode::{FwError, FwIrqSpec, FwNode, FwNodeId, FwNodeOps, FwProperty, FwReference};
use driver_os_api::{DmaInfo, DmaWindow};
use kspin::SpinNoPreempt;
use lazyinit::LazyInit;
use unittest::{assert_eq, def_test};

use super::*;
use crate::{DeviceDriver, DeviceState, IntxRouteStatus, PlatformIdentity};

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
fn test_resolved_dma_view_reaches_descriptor_and_device() {
    let bus = reset_model();
    let info = DmaInfo::new(0xffff_ffff, DmaWindow::IDENTITY, true).unwrap();
    let desc = device_desc_add(DeviceDescSpec {
        bus_id: bus.id(),
        parent: None,
        location: DeviceLocation::PlatformStatic { id: 4 },
        origin: DiscoveryOrigin::PlatformStatic,
        identity: DeviceIdentity::Platform(PlatformIdentity {
            alias: Some("matched"),
            firmware_id: None,
        }),
        transport: None,
        resources: ResourceSet::new(),
        dma: Some(info),
        intx: None,
        firmware_node: None,
    })
    .unwrap();
    assert_eq!(desc.dma(), Some(info));

    register_driver_object(Arc::new(MatchingDriver));

    let records = crate::device_records_snapshot();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].dma, Some(info));
    let device = crate::find_device(records[0].id).unwrap();
    assert_eq!(device.dma(), Some(info));
}

#[def_test(serial)]
fn test_intx_status_reaches_descriptor_and_device() {
    let bus = reset_model();
    let desc = device_desc_add(DeviceDescSpec {
        bus_id: bus.id(),
        parent: None,
        location: DeviceLocation::PlatformStatic { id: 5 },
        origin: DiscoveryOrigin::PlatformStatic,
        identity: DeviceIdentity::Platform(PlatformIdentity {
            alias: Some("matched"),
            firmware_id: None,
        }),
        transport: None,
        resources: ResourceSet::new(),
        dma: None,
        intx: Some(IntxRouteStatus::NotConnected),
        firmware_node: None,
    })
    .unwrap();
    assert_eq!(desc.intx_route(), Some(IntxRouteStatus::NotConnected));

    register_driver_object(Arc::new(MatchingDriver));

    let records = crate::device_records_snapshot();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].intx, Some(IntxRouteStatus::NotConnected));
    let device = crate::find_device(records[0].id).unwrap();
    assert_eq!(device.intx_route(), Some(IntxRouteStatus::NotConnected));
}

#[def_test(serial)]
fn test_unmatched_descriptor_keeps_stable_discovered_object() {
    let bus = reset_model();
    let desc = device_desc_add(DeviceDescSpec {
        bus_id: bus.id(),
        parent: None,
        location: DeviceLocation::PlatformStatic { id: 1 },
        origin: DiscoveryOrigin::PlatformStatic,
        identity: DeviceIdentity::Platform(PlatformIdentity {
            alias: Some("unmatched"),
            firmware_id: None,
        }),
        transport: None,
        resources: ResourceSet::new(),
        dma: None,
        intx: None,
        firmware_node: None,
    })
    .unwrap();

    // Registration publishes the stable object immediately; it has no current
    // binding and projects `Discovered`.
    let records = crate::device_records_snapshot();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, desc.device_id());
    assert_eq!(records[0].state, DeviceState::Discovered);
    assert_eq!(records[0].driver_name, None);

    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Unmatched);
    let device = device_for_desc(desc.id()).expect("stable object survives an unmatched probe");
    assert_eq!(device.id(), desc.device_id());
    assert_eq!(device.state(), DeviceState::Discovered);
    assert!(device.binding_ref().is_none());
}

#[def_test(serial)]
fn test_driver_registration_scans_existing_descriptors() {
    let bus = reset_model();
    device_desc_add(DeviceDescSpec {
        bus_id: bus.id(),
        parent: None,
        location: DeviceLocation::PlatformStatic { id: 2 },
        origin: DiscoveryOrigin::PlatformStatic,
        identity: DeviceIdentity::Platform(PlatformIdentity {
            alias: Some("matched"),
            firmware_id: None,
        }),
        transport: None,
        resources: ResourceSet::new(),
        dma: None,
        intx: None,
        firmware_node: None,
    })
    .unwrap();

    assert_eq!(
        crate::device_records_snapshot()[0].state,
        DeviceState::Discovered
    );
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
    let dma = DmaInfo::new(0xffff_ffff, DmaWindow::IDENTITY, true).unwrap();

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
        dma: Some(dma),
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
    // Adoption now records the DMA view on the descriptor as well; the stored
    // discovery record and the runtime object must expose the same view.
    assert_eq!(descriptors[0].dma(), Some(dma));
    assert_eq!(device.dma(), Some(dma));
    // Adoption is a boot/runtime handoff and has no firmware discovery source.
    assert_eq!(device.firmware_node(), None);
}

/// Backend-local identity used by the fake firmware backend below.
const FAKE_NODE_RAW: u32 = 7;

/// Minimal static firmware backend for node-association tests.
struct FakeFwOps;

static FAKE_FW_OPS: FakeFwOps = FakeFwOps;

impl FakeFwOps {
    fn known(id: FwNodeId) -> Result<(), FwError> {
        if id == FwNodeId::new(FAKE_NODE_RAW) {
            Ok(())
        } else {
            Err(FwError::InvalidNode)
        }
    }
}

impl FwNodeOps<'static> for FakeFwOps {
    fn name(&self, id: FwNodeId) -> Option<&'static str> {
        Self::known(id).ok().map(|()| "fake-device")
    }

    fn parent(&self, id: FwNodeId) -> Option<FwNodeId> {
        Self::known(id).ok()?;
        None
    }

    fn first_child(&self, id: FwNodeId) -> Option<FwNodeId> {
        Self::known(id).ok()?;
        None
    }

    fn next_sibling(&self, id: FwNodeId) -> Option<FwNodeId> {
        Self::known(id).ok()?;
        None
    }

    fn is_available(&self, id: FwNodeId) -> Result<bool, FwError> {
        Self::known(id)?;
        Ok(true)
    }

    fn is_interrupt_controller(&self, id: FwNodeId) -> Result<bool, FwError> {
        Self::known(id)?;
        Ok(false)
    }

    fn firmware_id(&self, id: FwNodeId, index: usize) -> Option<&'static str> {
        Self::known(id).ok()?;
        (index == 0).then_some("fake,device")
    }

    fn property(&self, id: FwNodeId, _name: &str) -> Result<Option<FwProperty<'static>>, FwError> {
        Self::known(id)?;
        Ok(None)
    }

    fn mmio_resource(
        &self,
        id: FwNodeId,
        _index: usize,
    ) -> Result<Option<fwnode::FwMmioResource<'static>>, FwError> {
        Self::known(id)?;
        Ok(None)
    }

    fn irq_spec(
        &'static self,
        id: FwNodeId,
        _index: usize,
    ) -> Result<Option<FwIrqSpec<'static>>, FwError> {
        Self::known(id)?;
        Ok(None)
    }

    fn reference(
        &'static self,
        id: FwNodeId,
        _property: &str,
        _index: usize,
        _arg_cells: usize,
    ) -> Result<Option<FwReference<'static>>, FwError> {
        Self::known(id)?;
        Ok(None)
    }
}

fn fake_node() -> FwNode<'static> {
    FwNode::new(&FAKE_FW_OPS, FwNodeId::new(FAKE_NODE_RAW))
}

/// Node observed by a probe, encoded as its raw backend id.
///
/// `u32::MAX` records "no associated node" so a stale value from a previous
/// test cannot be mistaken for an association.
static OBSERVED_NODE: AtomicU32 = AtomicU32::new(u32::MAX);

fn record_probe_node(device: &DeviceObject) {
    let observed = device
        .firmware_node()
        .map_or(u32::MAX, |node| node.id().raw());
    OBSERVED_NODE.store(observed, Ordering::SeqCst);
}

struct RecordingDriver;
struct RetryRecordingDriver;
struct FailingDriver;

impl DeviceDriver for RecordingDriver {
    fn name(&self) -> &'static str {
        "node-recording-test"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn crate::DeviceMatcher {
        static M: crate::CompatibleAliasMatcher = crate::CompatibleAliasMatcher("node-recorded");
        &M
    }

    fn probe_device(&self, device: Arc<DeviceObject>) -> DriverResult<()> {
        record_probe_node(&device);
        Ok(())
    }
}

impl DeviceDriver for RetryRecordingDriver {
    fn name(&self) -> &'static str {
        "node-retry-recording-test"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn crate::DeviceMatcher {
        static M: crate::CompatibleAliasMatcher = crate::CompatibleAliasMatcher("node-retry");
        &M
    }

    fn probe_device(&self, device: Arc<DeviceObject>) -> DriverResult<()> {
        record_probe_node(&device);
        Ok(())
    }
}

impl DeviceDriver for FailingDriver {
    fn name(&self) -> &'static str {
        "node-failing-test"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn crate::DeviceMatcher {
        static M: crate::CompatibleAliasMatcher = crate::CompatibleAliasMatcher("node-retry");
        &M
    }

    fn probe_device(&self, device: Arc<DeviceObject>) -> DriverResult<()> {
        // The first, failing probe must already see the discovery association.
        record_probe_node(&device);
        Err(DriverError::Io)
    }
}

fn node_desc(
    bus: &BusInstance,
    location_id: u16,
    alias: &'static str,
    node: Option<FwNode<'static>>,
) -> DeviceDesc {
    device_desc_add(DeviceDescSpec {
        bus_id: bus.id(),
        parent: None,
        location: DeviceLocation::PlatformStatic { id: location_id },
        origin: DiscoveryOrigin::PlatformStatic,
        identity: DeviceIdentity::Platform(PlatformIdentity {
            alias: Some(alias),
            firmware_id: None,
        }),
        transport: None,
        resources: ResourceSet::new(),
        dma: None,
        intx: None,
        firmware_node: node,
    })
    .unwrap()
}

#[def_test(serial)]
fn test_firmware_node_reaches_descriptor_object_and_probe() {
    let bus = reset_model();
    OBSERVED_NODE.store(u32::MAX, Ordering::SeqCst);
    register_driver_object(Arc::new(RecordingDriver));

    let node = fake_node();
    let desc = node_desc(&bus, 10, "node-recorded", Some(node));

    assert_eq!(
        probe_device_desc(desc.id()).unwrap(),
        ProbeOutcome::Activated
    );
    assert_eq!(OBSERVED_NODE.load(Ordering::SeqCst), FAKE_NODE_RAW);

    let bound = crate::device_registry()
        .find_device_for_desc(desc.id())
        .unwrap();
    assert_eq!(bound.firmware_node(), Some(node));
}

#[def_test(serial)]
fn test_absent_firmware_node_stays_absent() {
    let bus = reset_model();
    OBSERVED_NODE.store(FAKE_NODE_RAW, Ordering::SeqCst);
    register_driver_object(Arc::new(RecordingDriver));

    let desc = node_desc(&bus, 11, "node-recorded", None);

    assert_eq!(
        probe_device_desc(desc.id()).unwrap(),
        ProbeOutcome::Activated
    );
    assert_eq!(OBSERVED_NODE.load(Ordering::SeqCst), u32::MAX);

    let bound = crate::device_registry()
        .find_device_for_desc(desc.id())
        .unwrap();
    assert_eq!(bound.firmware_node(), None);
}

#[def_test(serial)]
fn test_failed_probe_retry_preserves_descriptor_and_node() {
    let bus = reset_model();
    OBSERVED_NODE.store(u32::MAX, Ordering::SeqCst);
    register_driver_object(Arc::new(FailingDriver));

    let node = fake_node();
    let desc = node_desc(&bus, 12, "node-retry", Some(node));

    assert_eq!(
        probe_device_desc(desc.id()).unwrap(),
        ProbeOutcome::Unclaimed
    );
    // A failed round clears the binding but keeps the stable object, so the
    // retry below must observe the same object and descriptor identity.
    let failed = crate::device_registry()
        .find_device_for_desc(desc.id())
        .expect("stable object survives a failed round");
    assert_eq!(failed.id(), desc.device_id());
    assert_eq!(failed.state(), DeviceState::Discovered);
    assert!(failed.binding_ref().is_none());
    assert_eq!(OBSERVED_NODE.load(Ordering::SeqCst), FAKE_NODE_RAW);

    // Reset the observation so the retry assertion below cannot pass on the
    // value left by the failed probe.
    OBSERVED_NODE.store(u32::MAX, Ordering::SeqCst);

    // The registry keeps the descriptor copy, so binding it through a second
    // driver must rebuild the same association rather than re-discover it.
    register_driver_object(Arc::new(RetryRecordingDriver));

    assert_eq!(OBSERVED_NODE.load(Ordering::SeqCst), FAKE_NODE_RAW);
    let bound = crate::device_registry()
        .find_device_for_desc(desc.id())
        .unwrap();
    assert_eq!(bound.id(), failed.id());
    assert_eq!(bound.firmware_node(), Some(node));
    assert_eq!(
        crate::device_registry().find_device_desc(desc.id()).unwrap().id(),
        desc.id()
    );
}

// ---------------------------------------------------------------------------
// Candidate ordering, rollback, and binding-lock fixtures.
// ---------------------------------------------------------------------------

type ProbeFn = dyn Fn(Arc<DeviceObject>) -> DriverResult<()> + Send + Sync;

struct TestDriver<Matcher> {
    name: &'static str,
    bus_types: &'static [BusTypeId],
    matcher: Matcher,
    probe: Arc<ProbeFn>,
    remove: Option<Arc<ProbeFn>>,
}

impl<Matcher: crate::DeviceMatcher> DeviceDriver for TestDriver<Matcher> {
    fn name(&self) -> &'static str {
        self.name
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        self.bus_types
    }

    fn matcher(&self) -> &dyn crate::DeviceMatcher {
        &self.matcher
    }

    fn probe_device(&self, device: Arc<DeviceObject>) -> DriverResult<()> {
        (self.probe)(device)
    }

    fn remove(&self, device: Arc<DeviceObject>) -> DriverResult<()> {
        match &self.remove {
            Some(remove) => remove(device),
            None => Ok(()),
        }
    }
}

fn platform_driver(
    name: &'static str,
    matcher: impl crate::DeviceMatcher,
    probe: impl Fn(Arc<DeviceObject>) -> DriverResult<()> + Send + Sync + 'static,
) -> Arc<dyn DeviceDriver> {
    platform_driver_with_remove(name, matcher, probe, |_| Ok(()))
}

fn platform_driver_with_remove(
    name: &'static str,
    matcher: impl crate::DeviceMatcher,
    probe: impl Fn(Arc<DeviceObject>) -> DriverResult<()> + Send + Sync + 'static,
    remove: impl Fn(Arc<DeviceObject>) -> DriverResult<()> + Send + Sync + 'static,
) -> Arc<dyn DeviceDriver> {
    Arc::new(TestDriver {
        name,
        bus_types: &[BusTypeId::PLATFORM],
        matcher,
        probe: Arc::new(probe),
        remove: Some(Arc::new(remove)),
    })
}

struct CountingMatcher {
    alias: &'static str,
    calls: Arc<AtomicUsize>,
}

impl crate::DeviceMatcher for CountingMatcher {
    fn matches(&self, desc: &DeviceDesc) -> MatchResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if desc.identity()
            == DeviceIdentity::Platform(PlatformIdentity {
                alias: Some(self.alias),
                firmware_id: None,
            })
        {
            MatchResult::Match
        } else {
            MatchResult::NoMatch
        }
    }
}

static TRACE: LazyInit<SpinNoPreempt<Vec<&'static str>>> = LazyInit::new();

fn trace_slot() -> &'static SpinNoPreempt<Vec<&'static str>> {
    TRACE.call_once(|| SpinNoPreempt::new(Vec::new()));
    TRACE.get().unwrap()
}

fn trace_reset() {
    trace_slot().lock().clear();
}

fn trace_push(entry: &'static str) {
    trace_slot().lock().push(entry);
}

fn trace_snapshot() -> Vec<&'static str> {
    trace_slot().lock().clone()
}

fn platform_desc(bus: &BusInstance, location_id: u16, alias: &'static str) -> DeviceDesc {
    device_desc_add(DeviceDescSpec {
        bus_id: bus.id(),
        parent: None,
        location: DeviceLocation::PlatformStatic { id: location_id },
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

fn device_for_desc(id: DeviceDescId) -> Option<Arc<DeviceObject>> {
    crate::device_registry().find_device_for_desc(id)
}

macro_rules! assert_desc_pending {
    ($id:expr) => {
        assert_eq!(
            crate::device_registry().device_desc_state($id),
            Some(crate::registry::DeviceDescState::Pending)
        );
    };
}

macro_rules! assert_trace {
    ($expected:expr) => {{
        let trace = trace_snapshot();
        assert_eq!(trace.as_slice(), $expected);
    }};
}

#[def_test(serial)]
fn test_first_matching_driver_wins_in_registration_order() {
    let bus = reset_model();
    trace_reset();

    let a = register_driver_object(platform_driver(
        "ordered-a",
        crate::CompatibleAliasMatcher("ordered"),
        |_| {
            trace_push("A-probe");
            Ok(())
        },
    ));
    let b = register_driver_object(platform_driver(
        "ordered-b",
        crate::CompatibleAliasMatcher("ordered"),
        |_| {
            trace_push("B-probe");
            Ok(())
        },
    ));
    let c = register_driver_object(platform_driver(
        "ordered-c",
        crate::CompatibleAliasMatcher("ordered"),
        |_| {
            trace_push("C-probe");
            Ok(())
        },
    ));

    let desc = platform_desc(&bus, 20, "ordered");
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Activated);
    assert_trace!(&["A-probe"]);
    assert_eq!(
        a.probe_stats(),
        crate::ProbeStats {
            attempts: 1,
            failures: 0,
        }
    );
    assert_eq!(b.probe_stats().attempts, 0);
    assert_eq!(c.probe_stats().attempts, 0);
}

#[def_test(serial)]
fn test_unmatched_descriptor_does_not_call_probe() {
    let bus = reset_model();
    trace_reset();
    let driver = register_driver_object(platform_driver(
        "unmatched-no-probe",
        crate::CompatibleAliasMatcher("other-alias"),
        |_| {
            trace_push("probe");
            Ok(())
        },
    ));

    let desc = platform_desc(&bus, 21, "wanted-alias");
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Unmatched);
    assert_eq!(driver.probe_stats().attempts, 0);
    assert!(trace_snapshot().is_empty());
    // Registration publishes the stable object even when no driver matches;
    // it stays Discovered with no binding.
    let records = crate::device_records_snapshot();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, desc.device_id());
    assert_eq!(records[0].state, DeviceState::Discovered);
    assert!(records[0].driver_id.is_none());
    assert!(
        crate::find_device(desc.device_id())
            .unwrap()
            .binding_ref()
            .is_none()
    );
}

#[def_test(serial)]
fn test_overlapping_matchers_use_registration_order() {
    let bus = reset_model();
    trace_reset();

    let _first = register_driver_object(platform_driver(
        "overlap-first",
        crate::CompatibleAliasMatcher("overlap"),
        |_| {
            trace_push("first");
            Ok(())
        },
    ));
    let _second = register_driver_object(platform_driver(
        "overlap-second",
        crate::CompatibleAliasMatcher("overlap"),
        |_| {
            trace_push("second");
            Ok(())
        },
    ));

    let desc = platform_desc(&bus, 22, "overlap");
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Activated);
    assert_trace!(&["first"]);
    assert_eq!(
        device_for_desc(desc.id())
            .unwrap()
            .driver_name(),
        Some("overlap-first")
    );
}

#[def_test(serial)]
fn test_failed_candidate_cleanup_precedes_next_probe() {
    let bus = reset_model();
    trace_reset();

    let a = register_driver_object(platform_driver(
        "cleanup-a",
        crate::CompatibleAliasMatcher("cleanup"),
        |device| {
            trace_push("A-probe");
            let binding = device
                .binding_ref_for_lifecycle_write()
                .expect("probe holds the binding write context");
            binding
                .add_cleanup(&device, || trace_push("A-cleanup"))
                .expect("probe may register cleanup");
            Err(DriverError::Io)
        },
    ));
    let b = register_driver_object(platform_driver(
        "cleanup-b",
        crate::CompatibleAliasMatcher("cleanup"),
        |_| {
            trace_push("B-probe");
            Ok(())
        },
    ));

    let desc = platform_desc(&bus, 23, "cleanup");
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Activated);
    assert_trace!(&["A-probe", "A-cleanup", "B-probe"]);
    assert_eq!(
        a.probe_stats(),
        crate::ProbeStats {
            attempts: 1,
            failures: 1,
        }
    );
    assert_eq!(
        b.probe_stats(),
        crate::ProbeStats {
            attempts: 1,
            failures: 0,
        }
    );
    assert_eq!(
        bus.probe_stats(),
        crate::ProbeStats {
            attempts: 2,
            failures: 1,
        }
    );
    assert!(a.bound_devices_snapshot().is_empty());

    let bound = device_for_desc(desc.id()).unwrap();
    let bound_on_b = b.bound_devices_snapshot();
    assert_eq!(bound_on_b.as_slice(), [bound.id()]);
    assert_eq!(bus.devices_snapshot().len(), 1);
    assert_eq!(bus.devices_snapshot()[0].id(), bound.id());
    let driver_ids = bus
        .drivers_snapshot()
        .iter()
        .map(|driver| driver.id())
        .collect::<Vec<_>>();
    assert_eq!(driver_ids.as_slice(), [b.id()]);
}

#[def_test(serial)]
fn test_all_candidates_fail_leaves_no_runtime_residue() {
    let bus = reset_model();
    trace_reset();

    let a = register_driver_object(platform_driver(
        "allfail-a",
        crate::CompatibleAliasMatcher("allfail"),
        |device| {
            trace_push("A-probe");
            let binding = device
                .binding_ref_for_lifecycle_write()
                .expect("probe holds the binding write context");
            binding
                .add_cleanup(&device, || trace_push("A-cleanup"))
                .expect("probe may register cleanup");
            Err(DriverError::Io)
        },
    ));
    let b = register_driver_object(platform_driver(
        "allfail-b",
        crate::CompatibleAliasMatcher("allfail"),
        |device| {
            trace_push("B-probe");
            let binding = device
                .binding_ref_for_lifecycle_write()
                .expect("probe holds the binding write context");
            binding
                .add_cleanup(&device, || trace_push("B-cleanup"))
                .expect("probe may register cleanup");
            Err(DriverError::Io)
        },
    ));

    let desc = platform_desc(&bus, 24, "allfail");
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Unclaimed);
    assert_trace!(&["A-probe", "A-cleanup", "B-probe", "B-cleanup"]);
    // All bindings are retired, but the stable registration object stays.
    let records = crate::device_records_snapshot();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, desc.device_id());
    assert_eq!(records[0].state, DeviceState::Discovered);
    assert_eq!(records[0].driver_name, None);
    let device = device_for_desc(desc.id()).expect("stable object survives an all-fail round");
    assert!(device.binding_ref().is_none());
    assert!(a.bound_devices_snapshot().is_empty());
    assert!(b.bound_devices_snapshot().is_empty());
    assert!(bus.devices_snapshot().is_empty());
    assert!(bus.drivers_snapshot().is_empty());
    assert_desc_pending!(desc.id());
}

#[def_test(serial)]
fn test_explicit_retry_reruns_original_drivers() {
    let bus = reset_model();
    trace_reset();

    register_driver_object(platform_driver(
        "retry-a",
        crate::CompatibleAliasMatcher("retry"),
        |_| {
            trace_push("A-probe");
            Err(DriverError::Io)
        },
    ));
    register_driver_object(platform_driver(
        "retry-b",
        crate::CompatibleAliasMatcher("retry"),
        |_| {
            trace_push("B-probe");
            Err(DriverError::Io)
        },
    ));

    let desc = platform_desc(&bus, 25, "retry");
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Unclaimed);
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Unclaimed);
    assert_trace!(&["A-probe", "B-probe", "A-probe", "B-probe"]);
    assert_desc_pending!(desc.id());
}

#[def_test(serial)]
fn test_device_first_and_driver_first_share_binding() {
    let bus = reset_model();

    let a = register_driver_object(platform_driver(
        "route-a",
        crate::CompatibleAliasMatcher("route-a"),
        |_| Ok(()),
    ));
    let desc_a = platform_desc(&bus, 30, "route-a");
    let lock_a_before = crate::device_registry()
        .binding_for_desc(desc_a.id())
        .unwrap();
    assert_eq!(
        probe_device_desc(desc_a.id()).unwrap(),
        ProbeOutcome::Activated
    );
    let lock_a_after = crate::device_registry()
        .binding_for_desc(desc_a.id())
        .unwrap();
    assert!(Arc::ptr_eq(&lock_a_before, &lock_a_after));
    let device_a = device_for_desc(desc_a.id()).unwrap();
    assert_eq!(device_a.driver_name(), Some("route-a"));
    assert_eq!(device_a.state(), DeviceState::Active);

    let desc_b = platform_desc(&bus, 31, "route-b");
    let lock_b_before = crate::device_registry()
        .binding_for_desc(desc_b.id())
        .unwrap();
    let _b = register_driver_object(platform_driver(
        "route-b",
        crate::CompatibleAliasMatcher("route-b"),
        |_| Ok(()),
    ));
    let lock_b_after = crate::device_registry()
        .binding_for_desc(desc_b.id())
        .unwrap();
    assert!(Arc::ptr_eq(&lock_b_before, &lock_b_after));
    let device_b = device_for_desc(desc_b.id()).unwrap();
    assert_eq!(device_b.driver_name(), Some("route-b"));
    assert_eq!(device_b.state(), DeviceState::Active);
    let bound_on_a = a.bound_devices_snapshot();
    assert_eq!(bound_on_a.as_slice(), [device_a.id()]);
}

#[def_test(serial)]
fn test_binding_lock_survives_candidate_rebuild_and_requeue() {
    let bus = reset_model();

    register_driver_object(platform_driver(
        "lock-a",
        crate::CompatibleAliasMatcher("lock"),
        |_| Err(DriverError::Io),
    ));
    register_driver_object(platform_driver(
        "lock-b",
        crate::CompatibleAliasMatcher("lock"),
        |_| Err(DriverError::Io),
    ));

    let desc = platform_desc(&bus, 32, "lock");
    let initial = crate::device_registry()
        .binding_for_desc(desc.id())
        .unwrap();
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Unclaimed);
    let after_first = crate::device_registry()
        .binding_for_desc(desc.id())
        .unwrap();
    assert!(Arc::ptr_eq(&initial, &after_first));

    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Unclaimed);
    let after_retry = crate::device_registry()
        .binding_for_desc(desc.id())
        .unwrap();
    assert!(Arc::ptr_eq(&initial, &after_retry));
    assert_desc_pending!(desc.id());
}

#[def_test(serial)]
fn test_new_driver_does_not_preempt_bound_device() {
    let bus = reset_model();

    let _a = register_driver_object(platform_driver(
        "preempt-a",
        crate::CompatibleAliasMatcher("preempt"),
        |_| Ok(()),
    ));
    let desc = platform_desc(&bus, 33, "preempt");
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Activated);

    let b = register_driver_object(platform_driver(
        "preempt-b",
        crate::CompatibleAliasMatcher("preempt"),
        |_| Ok(()),
    ));
    assert_eq!(b.probe_stats().attempts, 0);
    assert!(b.bound_devices_snapshot().is_empty());
    assert_eq!(
        device_for_desc(desc.id())
            .unwrap()
            .driver_name(),
        Some("preempt-a")
    );
}

#[def_test(serial)]
fn test_scan_includes_probing_descriptor() {
    let bus = reset_model();

    let pending = platform_desc(&bus, 40, "scan-pending");
    let probing = platform_desc(&bus, 41, "scan-probing");
    let binding = crate::device_registry()
        .binding_for_desc(probing.id())
        .unwrap();
    let guard = binding.lock();
    assert!(
        crate::device_registry()
            .mark_device_desc_probing(probing.id())
            .is_some()
    );

    let awaiting = crate::device_registry()
        .descriptors_awaiting_binding(&[BusTypeId::PLATFORM]);
    let ids = awaiting.iter().map(|entry| entry.id).collect::<Vec<_>>();
    assert!(ids.contains(&pending.id()));
    assert!(ids.contains(&probing.id()));

    drop(guard);
    crate::device_registry().requeue_device_desc(probing.id());
}

#[def_test(serial)]
fn test_same_descriptor_registration_is_diagnosed_locally() {
    let bus = reset_model();
    trace_reset();

    let registrations = Arc::new(AtomicUsize::new(0));
    let registrations_in_probe = registrations.clone();
    register_driver_object(platform_driver(
        "local-a",
        crate::CompatibleAliasMatcher("local"),
        move |_| {
            trace_push("A-probe");
            if registrations_in_probe.fetch_add(1, Ordering::SeqCst) == 0 {
                register_driver_object(platform_driver(
                    "local-b",
                    crate::CompatibleAliasMatcher("local"),
                    |_| {
                        trace_push("B-probe");
                        Ok(())
                    },
                ));
            }
            Err(DriverError::Io)
        },
    ));

    let desc = platform_desc(&bus, 42, "local");
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Unclaimed);
    assert_trace!(&["A-probe"]);
    let b = crate::find_driver_by_name("local-b").unwrap();
    assert_eq!(b.probe_stats().attempts, 0);
    assert!(b.bound_devices_snapshot().is_empty());

    // The explicit retry is the only path that lets B attempt this
    // descriptor; the local diagnostic never auto-retries it.
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Activated);
    assert_trace!(&["A-probe", "A-probe", "B-probe"]);
    assert_eq!(
        device_for_desc(desc.id())
            .unwrap()
            .driver_name(),
        Some("local-b")
    );
}

static CHILD_DESC: AtomicU64 = AtomicU64::new(u64::MAX);

#[def_test(serial)]
fn test_probe_registers_child_without_binding_lock() {
    let bus = reset_model();
    let bus_id = bus.id();
    CHILD_DESC.store(u64::MAX, Ordering::SeqCst);

    register_driver_object(platform_driver(
        "child-parent",
        crate::CompatibleAliasMatcher("parent"),
        move |_| {
            let child = device_desc_add(DeviceDescSpec {
                bus_id,
                parent: None,
                location: DeviceLocation::PlatformStatic { id: 43 },
                origin: DiscoveryOrigin::PlatformStatic,
                identity: DeviceIdentity::Platform(PlatformIdentity {
                    alias: Some("child"),
                    firmware_id: None,
                }),
                transport: None,
                resources: ResourceSet::new(),
                dma: None,
                intx: None,
                firmware_node: None,
            })
            .unwrap();
            CHILD_DESC.store(child.id().raw(), Ordering::SeqCst);
            Ok(())
        },
    ));

    let desc = platform_desc(&bus, 44, "parent");
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Activated);

    let child_id = DeviceDescId::new(CHILD_DESC.load(Ordering::SeqCst));
    assert_desc_pending!(child_id);
    // Registration publishes the child's stable object even though the probe
    // that created it never held the child's binding lock.
    let child = device_for_desc(child_id).expect("registered child has a stable object");
    assert_eq!(child.state(), DeviceState::Discovered);
}

#[def_test(serial)]
fn test_nonmatching_driver_evaluated_once_per_round() {
    let bus = reset_model();
    trace_reset();

    let a = register_driver_object(platform_driver(
        "round-a",
        crate::CompatibleAliasMatcher("round"),
        |_| {
            trace_push("A-probe");
            Err(DriverError::Io)
        },
    ));
    let calls = Arc::new(AtomicUsize::new(0));
    let b = register_driver_object(platform_driver(
        "round-b",
        CountingMatcher {
            alias: "never",
            calls: calls.clone(),
        },
        |_| {
            trace_push("B-probe");
            Ok(())
        },
    ));

    let desc = platform_desc(&bus, 45, "round");
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Unclaimed);
    assert_eq!(a.probe_stats().attempts, 1);
    assert_eq!(b.probe_stats().attempts, 0);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Unclaimed);
    assert_eq!(a.probe_stats().attempts, 2);
    assert_eq!(b.probe_stats().attempts, 0);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_trace!(&["A-probe", "A-probe"]);
}

#[def_test(serial)]
fn test_core_preparation_failure_is_an_error() {
    let bus = reset_model();
    trace_reset();

    let a = register_driver_object(platform_driver(
        "core-a",
        crate::CompatibleAliasMatcher("core"),
        |device| {
            trace_push("A-probe");
            let binding = device
                .binding_ref_for_lifecycle_write()
                .expect("probe holds the binding write context");
            binding
                .add_cleanup(&device, || trace_push("A-cleanup"))
                .expect("probe may register cleanup");
            Err(DriverError::Io)
        },
    ));
    let b = register_driver_object(platform_driver(
        "core-b",
        crate::CompatibleAliasMatcher("core"),
        |_| {
            trace_push("B-probe");
            Ok(())
        },
    ));

    let desc = platform_desc(&bus, 46, "core");
    // Let A's candidate be built and probed; fail B's construction.
    crate::test_hooks::fail_candidate_setup_after(1);
    assert_eq!(
        probe_device_desc(desc.id()),
        Err(DriverError::NoMemory)
    );
    assert_trace!(&["A-probe", "A-cleanup"]);
    // The core error aborts the round but never discards the registration.
    let records = crate::device_records_snapshot();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, desc.device_id());
    assert_eq!(records[0].state, DeviceState::Discovered);
    assert!(a.bound_devices_snapshot().is_empty());
    assert!(b.bound_devices_snapshot().is_empty());
    assert_desc_pending!(desc.id());
}

#[def_test(serial)]
fn test_no_candidate_does_not_report_success() {
    let bus = reset_model();
    let desc = platform_desc(&bus, 47, "no-candidate");
    let outcome = probe_device_desc(desc.id()).unwrap();
    assert_eq!(outcome, ProbeOutcome::Unmatched);
    assert!(!outcome.activated());
    let records = crate::device_records_snapshot();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].state, DeviceState::Discovered);
    let device = device_for_desc(desc.id()).unwrap();
    assert_eq!(device.state(), DeviceState::Discovered);
    assert!(device.binding_ref().is_none());
}

static NODE_OBSERVATIONS: LazyInit<SpinNoPreempt<Vec<u32>>> = LazyInit::new();

fn node_observation_slot() -> &'static SpinNoPreempt<Vec<u32>> {
    NODE_OBSERVATIONS.call_once(|| SpinNoPreempt::new(Vec::new()));
    NODE_OBSERVATIONS.get().unwrap()
}

fn record_observed_node(device: &DeviceObject) {
    node_observation_slot()
        .lock()
        .push(device.firmware_node().map_or(u32::MAX, |node| node.id().raw()));
}

#[def_test(serial)]
fn test_failed_attempts_keep_descriptor_identity_and_fwnode() {
    let bus = reset_model();
    node_observation_slot().lock().clear();

    register_driver_object(platform_driver(
        "node-fail",
        crate::CompatibleAliasMatcher("node-shared"),
        |device| {
            record_observed_node(&device);
            Err(DriverError::Io)
        },
    ));
    register_driver_object(platform_driver(
        "node-win",
        crate::CompatibleAliasMatcher("node-shared"),
        |device| {
            record_observed_node(&device);
            Ok(())
        },
    ));

    let node = fake_node();
    let desc = node_desc(&bus, 48, "node-shared", Some(node));
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Activated);
    let observed = node_observation_slot().lock();
    assert_eq!(observed.as_slice(), [FAKE_NODE_RAW, FAKE_NODE_RAW]);
    assert_eq!(
        crate::device_registry().find_device_desc(desc.id()).unwrap().id(),
        desc.id()
    );
    let bound = device_for_desc(desc.id()).unwrap();
    assert_eq!(bound.firmware_node(), Some(node));
    assert_eq!(bound.driver_name(), Some("node-win"));
}

static EVENT_TRACE: LazyInit<SpinNoPreempt<Vec<(crate::DeviceEventKind, DeviceId)>>> =
    LazyInit::new();

fn event_trace_slot() -> &'static SpinNoPreempt<Vec<(crate::DeviceEventKind, DeviceId)>> {
    EVENT_TRACE.call_once(|| SpinNoPreempt::new(Vec::new()));
    EVENT_TRACE.get().unwrap()
}

fn event_id(event: crate::DeviceEvent) -> DeviceId {
    match event {
        crate::DeviceEvent::Published { id }
        | crate::DeviceEvent::Matched { id }
        | crate::DeviceEvent::Removed { id }
        | crate::DeviceEvent::BindingRetired { id } => id,
        crate::DeviceEvent::Bound { id, .. } | crate::DeviceEvent::Activated { id, .. } => id,
    }
}

fn subscribe_all_events() {
    for kind in [
        crate::DeviceEventKind::Published,
        crate::DeviceEventKind::Matched,
        crate::DeviceEventKind::Bound,
        crate::DeviceEventKind::Activated,
        crate::DeviceEventKind::Removed,
        crate::DeviceEventKind::BindingRetired,
    ] {
        crate::subscribe_device_event_kind(
            kind,
            Arc::new(|event| event_trace_slot().lock().push((event.kind(), event_id(event)))),
        );
    }
}

static FAILED_EVENT_ID: AtomicU64 = AtomicU64::new(u64::MAX);

#[def_test(serial)]
fn test_failed_attempt_emits_only_binding_retired() {
    let bus = reset_model();
    event_trace_slot().lock().clear();
    subscribe_all_events();
    FAILED_EVENT_ID.store(u64::MAX, Ordering::SeqCst);

    register_driver_object(platform_driver(
        "events-fail",
        crate::CompatibleAliasMatcher("events"),
        |device| {
            FAILED_EVENT_ID.store(device.id().raw(), Ordering::SeqCst);
            Err(DriverError::Io)
        },
    ));
    register_driver_object(platform_driver(
        "events-win",
        crate::CompatibleAliasMatcher("events"),
        |_| Ok(()),
    ));

    let desc = platform_desc(&bus, 49, "events");
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Activated);

    let bound = device_for_desc(desc.id()).unwrap();
    // The failing and winning candidates share the stable device identity:
    // the failed attempt retires a binding, not a device.
    assert_eq!(
        DeviceId::new(FAILED_EVENT_ID.load(Ordering::SeqCst)),
        bound.id()
    );
    let trace = event_trace_slot().lock().clone();
    let kinds = trace
        .iter()
        .filter(|(_, id)| *id == bound.id())
        .map(|(kind, _)| *kind)
        .collect::<Vec<_>>();
    assert_eq!(
        kinds.as_slice(),
        [
            crate::DeviceEventKind::BindingRetired,
            crate::DeviceEventKind::Matched,
            crate::DeviceEventKind::Bound,
            crate::DeviceEventKind::Published,
            crate::DeviceEventKind::Activated,
        ]
    );
    // The failed candidate itself published nothing for any other identity.
    assert!(
        trace.iter().all(|(_, id)| *id == bound.id()),
        "failed candidate leaked an event: {trace:?}"
    );
}

static FAILED_DEVICE: LazyInit<SpinNoPreempt<Option<Arc<DeviceObject>>>> = LazyInit::new();
static FAILED_BINDING: LazyInit<SpinNoPreempt<Option<crate::BindingRef>>> = LazyInit::new();

fn failed_device_slot() -> &'static SpinNoPreempt<Option<Arc<DeviceObject>>> {
    FAILED_DEVICE.call_once(|| SpinNoPreempt::new(None));
    FAILED_DEVICE.get().unwrap()
}

fn failed_binding_slot() -> &'static SpinNoPreempt<Option<crate::BindingRef>> {
    FAILED_BINDING.call_once(|| SpinNoPreempt::new(None));
    FAILED_BINDING.get().unwrap()
}

#[def_test(serial)]
fn test_failed_binding_reference_cannot_follow_replacement() {
    let bus = reset_model();
    *failed_device_slot().lock() = None;
    *failed_binding_slot().lock() = None;

    register_driver_object(platform_driver(
        "terminal-fail",
        crate::CompatibleAliasMatcher("terminal"),
        |device| {
            *failed_binding_slot().lock() = device.binding_ref_for_lifecycle_write();
            *failed_device_slot().lock() = Some(device.clone());
            Err(DriverError::Io)
        },
    ));
    register_driver_object(platform_driver(
        "terminal-win",
        crate::CompatibleAliasMatcher("terminal"),
        |_| Ok(()),
    ));

    let desc = platform_desc(&bus, 50, "terminal");
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Activated);

    let device = failed_device_slot().lock().take().unwrap();
    let stale = failed_binding_slot()
        .lock()
        .take()
        .expect("failing probe captured a binding handle");
    let bound = device_for_desc(desc.id()).unwrap();

    // Replacement happens inside the same stable object, so the stale handle
    // is terminal and can never act on the replacement binding.
    assert_eq!(bound.id(), device.id());
    assert_eq!(stale.driver_name(), "terminal-fail");
    assert_eq!(stale.state(), DeviceState::Removed);
    assert!(stale.is_terminal());
    assert!(!stale.is_active());
    assert!(stale.try_acquire(&device).is_none());
    assert_eq!(
        stale.add_cleanup(&device, || {}),
        Err(DriverError::BadState)
    );

    let current = bound.binding_ref().expect("replacement binding is active");
    assert!(!stale.same_binding(&current));
    assert_eq!(current.driver_name(), "terminal-win");
    assert!(current.try_acquire(&bound).is_some());
    assert!(crate::find_device(device.id()).is_some());
}

/// A `BindingRef` may only add cleanups to the device whose slot currently
/// holds that exact runtime, even when the caller is in a valid lifecycle
/// write context for the *other* device.
#[def_test(serial)]
fn test_foreign_binding_cleanup_is_rejected_on_another_device() {
    let bus = reset_model();
    let foreign_cleanups = Arc::new(AtomicUsize::new(0));
    let own_cleanups = Arc::new(AtomicUsize::new(0));

    register_driver_object(platform_driver(
        "foreign-cleanup",
        crate::CompatibleAliasMatcher("foreign-cleanup"),
        |_| Ok(()),
    ));
    let a_desc = platform_desc(&bus, 60, "foreign-cleanup");
    let b_desc = platform_desc(&bus, 61, "foreign-cleanup");
    assert_eq!(
        probe_device_desc(a_desc.id()).unwrap(),
        ProbeOutcome::Activated
    );
    assert_eq!(
        probe_device_desc(b_desc.id()).unwrap(),
        ProbeOutcome::Activated
    );
    let a = device_for_desc(a_desc.id()).expect("A is active");
    let b = device_for_desc(b_desc.id()).expect("B is active");
    let a_binding = a.binding_ref().expect("A has an active binding");

    let (foreign_result, own_result) = crate::test_hooks::with_binding_lock(b.id(), || {
        let b_write = b
            .binding_ref_for_lifecycle_write()
            .expect("B holds a lifecycle write context");
        let foreign = foreign_cleanups.clone();
        let foreign_result = a_binding.add_cleanup(&b, move || {
            foreign.fetch_add(1, Ordering::SeqCst);
        });
        let own = own_cleanups.clone();
        let own_result = b_write.add_cleanup(&b, move || {
            own.fetch_add(1, Ordering::SeqCst);
        });
        (foreign_result, own_result)
    })
    .expect("B's binding lock is available");
    assert_eq!(
        foreign_result,
        Err(DriverError::BadState),
        "a foreign live binding must not register a cleanup on B"
    );
    own_result.expect("B's own binding may register a cleanup");

    remove_device_managed(b.id()).unwrap();
    assert_eq!(own_cleanups.load(Ordering::SeqCst), 1);
    assert_eq!(foreign_cleanups.load(Ordering::SeqCst), 0);

    remove_device_managed(a.id()).unwrap();
    assert_eq!(
        foreign_cleanups.load(Ordering::SeqCst),
        0,
        "the rejected cleanup must not run with A's retirement"
    );
}

/// A remove that waited on the binding lock must not tear down a binding
/// installed after its pre-wait observation.
#[def_test(serial)]
fn test_stale_remove_observation_does_not_tear_down_rebound_binding() {
    let bus = reset_model();
    register_driver_object(platform_driver(
        "stale-remove-a",
        crate::CompatibleAliasMatcher("stale-remove"),
        |_| Ok(()),
    ));
    let desc = platform_desc(&bus, 62, "stale-remove");
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Activated);
    let id = desc.device_id();
    let old = device_for_desc(desc.id())
        .expect("the first binding publishes an object")
        .binding_ref()
        .expect("the first binding is active");

    remove_device_managed(id).unwrap();
    register_driver_object(platform_driver(
        "stale-remove-b",
        crate::CompatibleAliasMatcher("stale-remove"),
        |_| Ok(()),
    ));
    // Registration scans the Pending descriptor; probe explicitly if that
    // scan did not run for this configuration.
    let outcome = probe_device_desc(desc.id()).unwrap();
    assert!(matches!(
        outcome,
        ProbeOutcome::Activated | ProbeOutcome::Skipped
    ));
    let device = device_for_desc(desc.id()).expect("the stable object is registered");
    let current = device.binding_ref().expect("the rebound binding is active");
    assert!(!current.same_binding(&old));
    assert_eq!(device.state(), DeviceState::Active);

    let binding_lock = crate::device_registry()
        .binding_for_desc(desc.id())
        .expect("the descriptor has a binding lock");
    {
        let _guard = binding_lock.lock();
        assert_eq!(
            remove_bound_device_locked(
                desc.id(),
                id,
                DescDisposition::Requeue,
                Some(&old),
            ),
            Err(DriverError::ResourceBusy),
            "a stale observation must not remove the replacement binding"
        );
    }

    let device = device_for_desc(desc.id()).expect("the device survives the stale remove");
    assert!(
        device
            .binding_ref()
            .is_some_and(|binding| binding.same_binding(&current)),
        "the replacement binding is still installed"
    );
    assert_eq!(device.state(), DeviceState::Active);
    remove_device_managed(id).unwrap();
}

static RECURSIVE_DESC: AtomicU64 = AtomicU64::new(u64::MAX);
static RECURSIVE_PROBES: AtomicUsize = AtomicUsize::new(0);
static INNER_OUTCOME: LazyInit<SpinNoPreempt<Option<ProbeOutcome>>> = LazyInit::new();

fn inner_outcome_slot() -> &'static SpinNoPreempt<Option<ProbeOutcome>> {
    INNER_OUTCOME.call_once(|| SpinNoPreempt::new(None));
    INNER_OUTCOME.get().unwrap()
}

#[def_test(serial)]
fn test_same_device_recursive_probe_returns_busy() {
    let bus = reset_model();
    RECURSIVE_PROBES.store(0, Ordering::SeqCst);
    *inner_outcome_slot().lock() = None;

    register_driver_object(platform_driver(
        "recursive",
        crate::CompatibleAliasMatcher("recursive"),
        |_| {
            RECURSIVE_PROBES.fetch_add(1, Ordering::SeqCst);
            let id = DeviceDescId::new(RECURSIVE_DESC.load(Ordering::SeqCst));
            *inner_outcome_slot().lock() = Some(probe_device_desc(id).unwrap());
            Ok(())
        },
    ));

    let desc = platform_desc(&bus, 51, "recursive");
    RECURSIVE_DESC.store(desc.id().raw(), Ordering::SeqCst);
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Activated);
    assert_eq!(*inner_outcome_slot().lock(), Some(ProbeOutcome::Busy));
    assert_eq!(RECURSIVE_PROBES.load(Ordering::SeqCst), 1);
}

#[def_test(serial)]
fn test_remove_unbinds_and_requeues_descriptor() {
    let bus = reset_model();
    trace_reset();

    let driver = register_driver_object(platform_driver(
        "remove-a",
        crate::CompatibleAliasMatcher("remove"),
        |device| {
            trace_push("probe");
            let binding = device
                .binding_ref_for_lifecycle_write()
                .expect("probe holds the binding write context");
            binding
                .add_cleanup(&device, || trace_push("cleanup"))
                .expect("probe may register cleanup");
            Ok(())
        },
    ));

    let desc = platform_desc(&bus, 52, "remove");
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Activated);
    let old_device = device_for_desc(desc.id()).unwrap();
    let old_id = old_device.id();
    let old_binding = old_device.binding_ref().expect("bound device captures a ref");

    assert_eq!(remove_device_managed(old_id), Ok(()));
    // Unbind keeps the stable registration object; only the binding slot is
    // cleared. The object stays registered with its discovery facts intact.
    let device = crate::find_device(old_id).expect("unbind keeps the stable object");
    assert_eq!(device.id(), old_id);
    assert_eq!(device.state(), DeviceState::Discovered);
    assert!(device.binding_ref().is_none());
    let records = crate::device_records_snapshot();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, old_id);
    assert_eq!(records[0].state, DeviceState::Discovered);
    assert_eq!(records[0].driver_name, None);
    assert!(bus.devices_snapshot().is_empty());
    assert!(bus.drivers_snapshot().is_empty());
    assert!(driver.bound_devices_snapshot().is_empty());
    assert_desc_pending!(desc.id());
    assert_trace!(&["probe", "cleanup"]);

    // The retired binding handle is terminal: cleanup has run and it cannot
    // admit hardware use.
    assert_eq!(old_binding.state(), DeviceState::Removed);
    assert!(old_binding.try_acquire(&device).is_none());

    // The descriptor and stable object identity survive unbind, and a fresh
    // binding is installed into the same object.
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Activated);
    let new_device = device_for_desc(desc.id()).unwrap();
    assert_eq!(new_device.id(), old_id);
    assert_eq!(new_device.state(), DeviceState::Active);
    let new_binding = new_device
        .binding_ref()
        .expect("rebound device captures a ref");
    assert!(!old_binding.same_binding(&new_binding));
    assert!(new_binding.try_acquire(&new_device).is_some());

    // Rebinding never revives the old binding or re-runs its cleanup: the
    // trace is the first probe, the old binding's cleanup, and the new probe.
    assert_eq!(old_binding.state(), DeviceState::Removed);
    assert!(old_binding.try_acquire(&device).is_none());
    assert_trace!(&["probe", "cleanup", "probe"]);
}

#[def_test(serial)]
fn test_unregister_pending_removes_registration_and_stable_object() {
    let bus = reset_model();
    trace_reset();
    event_trace_slot().lock().clear();
    subscribe_all_events();

    let matcher_calls = Arc::new(AtomicUsize::new(0));
    register_driver_object(platform_driver(
        "unreg-pending",
        CountingMatcher {
            alias: "unreg-pending",
            calls: matcher_calls.clone(),
        },
        |_| {
            trace_push("probe");
            Ok(())
        },
    ));

    let desc = platform_desc(&bus, 60, "unreg-pending");
    // Registration publishes the stable object even before the first probe.
    assert_eq!(
        crate::find_device(desc.device_id())
            .expect("registration publishes the stable object")
            .state(),
        DeviceState::Discovered
    );
    assert_eq!(crate::device_records_snapshot().len(), 1);

    assert_eq!(unregister_device_desc(desc.id()), Ok(()));
    assert!(crate::find_device(desc.device_id()).is_none());
    assert!(crate::find_device_desc(desc.id()).is_none());
    assert!(
        crate::device_descs_snapshot()
            .iter()
            .all(|registered| registered.id() != desc.id())
    );
    assert!(crate::device_records_snapshot().is_empty());
    assert!(trace_snapshot().is_empty());
    assert!(event_trace_slot().lock().is_empty());

    // A completed unregister removes the entry, so a later probe cannot even
    // acquire the binding handle: it is `InvalidInput`, not `Skipped`.
    assert_eq!(
        probe_device_desc(desc.id()),
        Err(DriverError::InvalidInput)
    );
    assert_eq!(matcher_calls.load(Ordering::SeqCst), 0);
    assert!(trace_snapshot().is_empty());

    // Sequential repeat is a missing ID, distinct from the concurrent overlap
    // that returns `Ok(())` after waiting on the lock.
    assert_eq!(
        unregister_device_desc(desc.id()),
        Err(DriverError::InvalidInput)
    );
}

#[def_test(serial)]
fn test_unregister_bound_runs_exactly_one_teardown() {
    let bus = reset_model();
    trace_reset();
    event_trace_slot().lock().clear();
    subscribe_all_events();

    let driver = register_driver_object(platform_driver_with_remove(
        "unreg-bound",
        crate::CompatibleAliasMatcher("unreg-bound"),
        |device| {
            trace_push("probe");
            let binding = device
                .binding_ref_for_lifecycle_write()
                .expect("probe holds the binding write context");
            binding
                .add_cleanup(&device, || trace_push("cleanup"))
                .expect("probe may register cleanup");
            Ok(())
        },
        |_| {
            trace_push("remove");
            Ok(())
        },
    ));

    let desc = platform_desc(&bus, 61, "unreg-bound");
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Activated);
    let device = device_for_desc(desc.id()).unwrap();
    let id = device.id();
    let binding = device.binding_ref().expect("bound device captures a ref");
    assert_eq!(
        event_trace_slot().lock().len(),
        4,
        "probe must emit Matched/Bound/Published/Activated"
    );
    event_trace_slot().lock().clear();

    assert_eq!(unregister_device_desc(desc.id()), Ok(()));
    assert!(crate::find_device(id).is_none());
    assert!(crate::find_device_desc(desc.id()).is_none());
    assert!(crate::device_records_snapshot().is_empty());
    assert!(bus.devices_snapshot().is_empty());
    assert!(bus.drivers_snapshot().is_empty());
    assert!(driver.bound_devices_snapshot().is_empty());
    assert_eq!(trace_snapshot(), alloc::vec!["probe", "remove", "cleanup"]);
    assert_eq!(device.state(), DeviceState::Removed);
    assert_eq!(device.driver_id(), None);
    assert!(device.binding_ref().is_none());
    assert_eq!(binding.state(), DeviceState::Removed);
    assert!(binding.try_acquire(&device).is_none());
    let events = event_trace_slot().lock().clone();
    assert_eq!(
        events.as_slice(),
        [(crate::DeviceEventKind::Removed, id)]
    );

    // The old identity is gone for good, not merely pending.
    assert_eq!(
        probe_device_desc(desc.id()),
        Err(DriverError::InvalidInput)
    );
    assert!(bus.devices_snapshot().is_empty());
}

#[def_test(serial)]
fn test_unregister_busy_with_live_device_use() {
    let bus = reset_model();
    trace_reset();

    register_driver_object(platform_driver(
        "unreg-busy",
        crate::CompatibleAliasMatcher("unreg-busy"),
        |device| {
            trace_push("probe");
            let binding = device
                .binding_ref_for_lifecycle_write()
                .expect("probe holds the binding write context");
            binding
                .add_cleanup(&device, || trace_push("cleanup"))
                .expect("probe may register cleanup");
            Ok(())
        },
    ));

    let desc = platform_desc(&bus, 62, "unreg-busy");
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Activated);
    let device = device_for_desc(desc.id()).unwrap();
    let id = device.id();
    let binding = device.binding_ref().expect("bound device captures a ref");
    let lease = binding
        .try_acquire(&device)
        .expect("active device admits a use");

    assert_eq!(
        unregister_device_desc(desc.id()),
        Err(DriverError::ResourceBusy)
    );
    assert_eq!(device.state(), DeviceState::Active);
    assert_eq!(device_for_desc(desc.id()).unwrap().id(), id);
    assert_eq!(crate::find_device(id).unwrap().state(), DeviceState::Active);
    assert_eq!(trace_snapshot(), alloc::vec!["probe"]);

    drop(lease);
    assert_eq!(unregister_device_desc(desc.id()), Ok(()));
    assert!(crate::find_device(id).is_none());
    assert!(crate::find_device_desc(desc.id()).is_none());
    assert_eq!(binding.state(), DeviceState::Removed);
    assert!(binding.try_acquire(&device).is_none());
    assert_eq!(trace_snapshot(), alloc::vec!["probe", "cleanup"]);
}

#[def_test(serial)]
fn test_terminal_state_is_removed_only() {
    use core::sync::atomic::AtomicBool;

    let bus = reset_model();
    register_driver_object(platform_driver(
        "terminal-state",
        crate::CompatibleAliasMatcher("terminal-state"),
        |_| Ok(()),
    ));
    let desc = platform_desc(&bus, 63, "terminal-state");
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Activated);
    let device = device_for_desc(desc.id()).unwrap();
    let binding = device.binding_ref().expect("bound device captures a ref");
    let lease = binding
        .try_acquire(&device)
        .expect("active device admits a use");

    // The busy-removal window is the only place a binding is `Removing`; it is
    // not terminal and a rollback must return it to `Active`.
    let paused = Arc::new(AtomicBool::new(false));
    let terminal_at_pause = Arc::new(AtomicBool::new(false));
    let active_at_pause = Arc::new(AtomicBool::new(false));
    {
        let paused = paused.clone();
        let terminal_at_pause = terminal_at_pause.clone();
        let active_at_pause = active_at_pause.clone();
        let binding = binding.clone();
        crate::test_hooks::install_pause_hook(Arc::new(move |point| {
            if matches!(
                point,
                crate::test_hooks::BindingPause::BindingRemovingBeforeUseCheck
            ) {
                paused.store(true, Ordering::SeqCst);
                terminal_at_pause.store(binding.is_terminal(), Ordering::SeqCst);
                active_at_pause.store(binding.is_active(), Ordering::SeqCst);
            }
        }));
    }

    assert_eq!(
        crate::remove_device_managed(device.id()),
        Err(DriverError::ResourceBusy)
    );
    crate::test_hooks::clear_pause_hook();
    assert!(paused.load(Ordering::SeqCst));
    assert!(
        !terminal_at_pause.load(Ordering::SeqCst),
        "a transient Removing binding is not terminal"
    );
    assert!(!active_at_pause.load(Ordering::SeqCst));
    assert_eq!(binding.state(), DeviceState::Active);
    assert!(!binding.is_terminal());

    drop(lease);
    crate::remove_device_managed(device.id()).unwrap();
    assert_eq!(binding.state(), DeviceState::Removed);
    assert!(binding.is_terminal());
}

static SAME_TASK_UNREGISTER_DESC: AtomicU64 = AtomicU64::new(u64::MAX);
static SAME_TASK_UNREGISTER_RESULT: LazyInit<SpinNoPreempt<Option<Result<(), DriverError>>>> =
    LazyInit::new();

fn same_task_unregister_result_slot() -> &'static SpinNoPreempt<Option<Result<(), DriverError>>> {
    SAME_TASK_UNREGISTER_RESULT.call_once(|| SpinNoPreempt::new(None));
    SAME_TASK_UNREGISTER_RESULT.get().unwrap()
}

#[def_test(serial)]
fn test_same_task_unregister_while_holding_binding_lock_is_busy() {
    let bus = reset_model();
    *same_task_unregister_result_slot().lock() = None;

    register_driver_object(platform_driver(
        "same-task-unreg",
        crate::CompatibleAliasMatcher("same-task-unreg"),
        |_| {
            let id = DeviceDescId::new(SAME_TASK_UNREGISTER_DESC.load(Ordering::SeqCst));
            *same_task_unregister_result_slot().lock() = Some(unregister_device_desc(id));
            Ok(())
        },
    ));

    let desc = platform_desc(&bus, 63, "same-task-unreg");
    SAME_TASK_UNREGISTER_DESC.store(desc.id().raw(), Ordering::SeqCst);
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Activated);

    assert_eq!(
        *same_task_unregister_result_slot().lock(),
        Some(Err(DriverError::ResourceBusy))
    );
    let device = device_for_desc(desc.id()).unwrap();
    assert_eq!(device.state(), DeviceState::Active);
    assert!(crate::find_device_desc(desc.id()).is_some());
}

#[def_test(serial)]
fn test_unbind_semantics_survive_and_unregister_is_final() {
    let bus = reset_model();
    trace_reset();

    register_driver_object(platform_driver(
        "unreg-after-unbind",
        crate::CompatibleAliasMatcher("unreg-after-unbind"),
        |device| {
            trace_push("probe");
            let binding = device
                .binding_ref_for_lifecycle_write()
                .expect("probe holds the binding write context");
            binding
                .add_cleanup(&device, || trace_push("cleanup"))
                .expect("probe may register cleanup");
            Ok(())
        },
    ));

    let desc = platform_desc(&bus, 64, "unreg-after-unbind");
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Activated);
    let device = device_for_desc(desc.id()).unwrap();
    let old_id = device.id();
    let old_binding = device.binding_ref().expect("bound device captures a ref");

    // The existing unbind path still requeues and permits a fresh binding on
    // the same stable object.
    assert_eq!(remove_device_managed(old_id), Ok(()));
    assert_desc_pending!(desc.id());
    assert_eq!(
        crate::find_device(old_id).unwrap().state(),
        DeviceState::Discovered
    );
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Activated);
    let new_device = device_for_desc(desc.id()).unwrap();
    let new_id = new_device.id();
    assert_eq!(new_id, old_id);
    let new_binding = new_device
        .binding_ref()
        .expect("rebound device captures a ref");
    assert!(!old_binding.same_binding(&new_binding));
    assert_eq!(old_binding.state(), DeviceState::Removed);

    // Unregister is final: no requeue, no rebinding, and both binding
    // generations stay terminal.
    assert_eq!(unregister_device_desc(desc.id()), Ok(()));
    assert!(crate::find_device(new_id).is_none());
    assert!(crate::find_device_desc(desc.id()).is_none());
    assert_eq!(new_binding.state(), DeviceState::Removed);
    assert!(old_binding.try_acquire(&device).is_none());
    assert!(new_binding.try_acquire(&device).is_none());
    assert_eq!(
        probe_device_desc(desc.id()),
        Err(DriverError::InvalidInput)
    );
}

#[def_test(serial)]
fn test_unregister_adopted_nodeless_device() {
    let bus = reset_model();
    let driver = register_driver_object(platform_driver(
        "unreg-adopt",
        crate::CompatibleAliasMatcher("unreg-adopt"),
        |_| Ok(()),
    ));

    let device = adopt_active_device(ActiveDeviceAdoption {
        bus_id: bus.id(),
        parent: None,
        location: DeviceLocation::PlatformStatic { id: 65 },
        origin: DiscoveryOrigin::PlatformStatic,
        identity: DeviceIdentity::Platform(PlatformIdentity {
            alias: Some("unreg-adopt"),
            firmware_id: None,
        }),
        transport: None,
        resources: ResourceSet::new(),
        dma: None,
        driver,
    })
    .unwrap();
    let id = device.id();
    assert!(device.firmware_node().is_none());
    let binding = device.binding_ref().expect("adopted device captures a ref");
    let desc_id = crate::device_descs_snapshot()[0].id();

    assert_eq!(unregister_device_desc(desc_id), Ok(()));
    assert!(crate::find_device(id).is_none());
    assert!(crate::find_device_desc(desc_id).is_none());
    assert_eq!(device.state(), DeviceState::Removed);
    assert!(device.binding_ref().is_none());
    assert_eq!(binding.state(), DeviceState::Removed);
    assert!(binding.try_acquire(&device).is_none());
}

#[def_test(serial)]
fn test_registry_descriptor_delete_requires_expected_state() {
    let bus = reset_model();
    let desc = platform_desc(&bus, 66, "delete-state");

    let wrong = crate::registry::DeviceDescState::Probing;
    assert!(
        crate::device_registry()
            .remove_device_desc_if_state(desc.id(), wrong)
            .is_none()
    );
    assert!(crate::find_device_desc(desc.id()).is_some());

    let removed = crate::device_registry()
        .remove_device_desc_if_state(desc.id(), crate::registry::DeviceDescState::Pending);
    assert!(removed.is_some());
    assert_eq!(removed.unwrap().id(), desc.id());
    assert!(crate::find_device_desc(desc.id()).is_none());
}

#[def_test(serial)]
fn test_bus_driver_listing_survives_shared_removal() {
    let bus = reset_model();

    let driver = register_driver_object(platform_driver(
        "shared-bus",
        crate::CompatibleAliasMatcher("shared-bus"),
        |_| Ok(()),
    ));

    let first_desc = platform_desc(&bus, 53, "shared-bus");
    let second_desc = platform_desc(&bus, 54, "shared-bus");
    assert_eq!(
        probe_device_desc(first_desc.id()).unwrap(),
        ProbeOutcome::Activated
    );
    assert_eq!(
        probe_device_desc(second_desc.id()).unwrap(),
        ProbeOutcome::Activated
    );
    let first_id = device_for_desc(first_desc.id()).unwrap().id();
    let second_id = device_for_desc(second_desc.id()).unwrap().id();

    assert_eq!(remove_device_managed(first_id), Ok(()));
    let driver_ids = bus
        .drivers_snapshot()
        .iter()
        .map(|driver| driver.id())
        .collect::<Vec<_>>();
    assert_eq!(driver_ids.as_slice(), [driver.id()]);
    let device_ids = bus
        .devices_snapshot()
        .iter()
        .map(|device| device.id())
        .collect::<Vec<_>>();
    assert_eq!(device_ids.as_slice(), [second_id]);

    assert_eq!(remove_device_managed(second_id), Ok(()));
    assert!(bus.drivers_snapshot().is_empty());
    assert!(bus.devices_snapshot().is_empty());
}

#[def_test(serial)]
fn test_bus_attach_sees_binding_metadata() {
    let bus = reset_model();

    let driver = register_driver_object(platform_driver(
        "attach-meta",
        crate::CompatibleAliasMatcher("attach-meta"),
        |_| Ok(()),
    ));

    let desc = platform_desc(&bus, 55, "attach-meta");
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Activated);
    let adopted = adopt_active_device(ActiveDeviceAdoption {
        bus_id: bus.id(),
        parent: None,
        location: DeviceLocation::PlatformStatic { id: 56 },
        origin: DiscoveryOrigin::PlatformStatic,
        identity: DeviceIdentity::Platform(PlatformIdentity {
            alias: Some("adopted-meta"),
            firmware_id: None,
        }),
        transport: None,
        resources: ResourceSet::new(),
        dma: None,
        driver,
    })
    .unwrap();

    assert_eq!(adopted.driver_name(), Some("attach-meta"));
    let listed = bus.devices_snapshot();
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().all(|device| device.driver_id().is_some()));
}
