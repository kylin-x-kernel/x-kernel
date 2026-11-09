// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, Ordering};

use driver_base::{DeviceKind, DriverError, DriverResult};
use fwnode::{FwError, FwIrqSpec, FwNode, FwNodeId, FwNodeOps, FwProperty, FwReference};
use driver_os_api::{DmaInfo, DmaWindow};
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
fn test_unmatched_descriptor_probe_does_not_create_runtime_object() {
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

    assert_eq!(crate::device_records_snapshot().len(), 0);
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Requeue);
    assert_eq!(crate::device_records_snapshot().len(), 0);
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
    assert!(
        crate::device_registry()
            .find_device_for_desc(desc.id())
            .is_none()
    );
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
    assert_eq!(bound.firmware_node(), Some(node));
    assert_eq!(
        crate::device_registry().find_device_desc(desc.id()).unwrap().id(),
        desc.id()
    );
}
