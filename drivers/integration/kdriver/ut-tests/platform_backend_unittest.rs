// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, Ordering};

use driver_base::{DeviceKind, DriverResult};
use kdevice::{DeviceDriver, DeviceMatcher, FirmwareMatchSpec, register_driver_object};
use khal::firmware::devices::{
    FirmwareDevice, FirmwareSource, FwError, FwIrqSpec, FwMmioResource, FwNode, FwNodeId,
    FwNodeOps, FwProperty, FwReference, FwSource, MmioResource,
};
use unittest::{assert_eq, def_test};

use super::*;

const NODE_RAW: u32 = 3;

/// Sentinel recording that no node reached the probe.
static PROBED_NODE: AtomicU32 = AtomicU32::new(u32::MAX);

/// Minimal single-node firmware backend used to build a `FirmwareDevice`.
struct FakeOps;

static FAKE_OPS: FakeOps = FakeOps;

impl FakeOps {
    fn known(id: FwNodeId) -> Result<(), FwError> {
        if id == FwNodeId::new(NODE_RAW) {
            Ok(())
        } else {
            Err(FwError::InvalidNode)
        }
    }
}

impl FwNodeOps<'static> for FakeOps {
    fn source(&self) -> FwSource {
        FwSource::DeviceTree
    }

    fn name(&self, id: FwNodeId) -> Option<&'static str> {
        Self::known(id).ok().map(|()| "fake-platform-device")
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
        (index == 0).then_some("test,platform")
    }

    fn property(&self, id: FwNodeId, _name: &str) -> Result<Option<FwProperty<'static>>, FwError> {
        Self::known(id)?;
        Ok(None)
    }

    fn mmio_resource(
        &self,
        id: FwNodeId,
        _index: usize,
    ) -> Result<Option<FwMmioResource<'static>>, FwError> {
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

static SPEC: FirmwareMatchSpec = FirmwareMatchSpec {
    alias: "test-platform",
    dt_compatibles: &["test,platform"],
    acpi_ids: &[],
};

struct RecordingDriver;

impl DeviceDriver for RecordingDriver {
    type Data = ();
    fn name(&self) -> &'static str {
        "platform-backend-recording-test"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }

    fn bus_types(&self) -> &'static [kdevice::BusTypeId] {
        &[kdevice::BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn DeviceMatcher {
        &SPEC
    }

    fn probe_device(&self, ctx: &kdevice::ProbeContext<'_>) -> DriverResult<()> {
        let device = ctx.device().clone();
        PROBED_NODE.store(
            device
                .firmware_node()
                .map_or(u32::MAX, |node| node.id().raw()),
            Ordering::SeqCst,
        );
        Ok(())
    }
}

fn device(firmware_id: &'static str, irq: Result<Option<FwIrqSpec<'static>>, FwError>) -> FirmwareDevice {
    FirmwareDevice {
        firmware_id,
        source: FirmwareSource::DeviceTree,
        node: FwNode::new(&FAKE_OPS, FwNodeId::new(NODE_RAW)),
        mmio: Ok(None),
        irq,
    }
}

fn setup() -> (PlatformBackend, EnumerationContext, kdevice::BusId) {
    let (_bus_type, bus) = kdevice::reset_and_setup_platform_bus_for_tests("platform-backend-bus");
    (PlatformBackend::new(), EnumerationContext::new(), bus.id())
}

#[def_test(serial)]
fn firmware_device_registration_preserves_node_association() {
    PROBED_NODE.store(u32::MAX, Ordering::SeqCst);
    let (mut backend, mut context, bus_id) = setup();
    register_driver_object(Arc::new(RecordingDriver));

    let dev = device("test,platform", Ok(None));
    backend
        .register_firmware_device(&mut context, bus_id, &dev)
        .unwrap();
    assert_eq!(context.registered_count(), 1);
    assert_eq!(context.probe_pending(), 1);
    assert_eq!(PROBED_NODE.load(Ordering::SeqCst), NODE_RAW);

    let records = kdevice::device_records_snapshot();
    assert_eq!(records.len(), 1);
    let device = kdevice::find_device(records[0].id).unwrap();
    assert_eq!(device.firmware_node(), Some(dev.node));
}

#[def_test(serial)]
fn firmware_device_helper_registers_described_mmio_without_transport() {
    use kdevice::{DeviceIdentity, DeviceLocation, MmioRegion, PlatformIdentity, ResourceDesc};

    let (mut backend, mut context, bus_id) = setup();

    let mut described = device(VIRTIO_MMIO_COMPATIBLE, Ok(None));
    described.mmio = Ok(Some(MmioResource {
        base: 0x2000_0000,
        size: 0x1000,
    }));
    backend
        .register_firmware_device(&mut context, bus_id, &described)
        .unwrap();
    assert_eq!(context.registered_count(), 1);

    let descs = kdevice::device_descs_snapshot();
    assert_eq!(descs.len(), 1);
    let desc = &descs[0];
    assert_eq!(
        desc.location(),
        DeviceLocation::Mmio {
            base: 0x2000_0000,
            size: 0x1000,
        }
    );
    assert_eq!(
        desc.identity(),
        DeviceIdentity::Platform(PlatformIdentity {
            alias: None,
            firmware_id: Some(VIRTIO_MMIO_COMPATIBLE),
        })
    );
    // Discovery records only the description; the VirtIO type is identified
    // later, during probe.
    assert_eq!(desc.transport(), None);
    assert_eq!(desc.resources().len(), 1);
    assert!(matches!(
        desc.resources()[0],
        ResourceDesc::Mmio(MmioRegion {
            base: 0x2000_0000,
            size: 0x1000,
        })
    ));
    // The descriptor-level firmware-node accessor is device-core-internal, so
    // the node association is covered by the probe-path test instead.
}

#[def_test(serial)]
fn firmware_device_helper_rejects_malformed_mmio_without_registering() {
    let (mut backend, mut context, bus_id) = setup();

    let mut malformed = device(VIRTIO_MMIO_COMPATIBLE, Ok(None));
    malformed.mmio = Err(FwError::Unsupported);
    assert!(backend
        .register_firmware_device(&mut context, bus_id, &malformed)
        .is_err());
    assert_eq!(context.registered_count(), 0);
    assert!(kdevice::device_descs_snapshot().is_empty());

    // A later, well-formed node is still registered: the failure is reported
    // to the caller, which keeps the first error while enumeration continues.
    let valid = device(VIRTIO_MMIO_COMPATIBLE, Ok(None));
    assert!(backend
        .register_firmware_device(&mut context, bus_id, &valid)
        .is_ok());
    assert_eq!(context.registered_count(), 1);
    assert_eq!(kdevice::device_descs_snapshot().len(), 1);
}

#[def_test(serial)]
fn firmware_device_helper_registers_absent_mmio_and_reports_failure() {
    use kdevice::DeviceLocation;

    let (mut backend, mut context, bus_id) = setup();

    // A virtio-mmio node without an MMIO range is still described: probe is
    // responsible for rejecting it, so discovery registers a firmware-node
    // location instead of skipping the node.
    let absent_mmio = device(VIRTIO_MMIO_COMPATIBLE, Ok(None));
    assert!(backend
        .register_firmware_device(&mut context, bus_id, &absent_mmio)
        .is_ok());
    assert_eq!(context.registered_count(), 1);
    let descs = kdevice::device_descs_snapshot();
    assert_eq!(descs.len(), 1);
    assert!(matches!(
        descs[0].location(),
        DeviceLocation::FirmwareNode { .. }
    ));
    assert_eq!(descs[0].transport(), None);

    // Failure: a malformed IRQ declaration is reported to the caller, which
    // aggregates the first error, and leaves no additional descriptor behind.
    let failed = device("test,platform", Err(FwError::InvalidValue));
    assert!(backend
        .register_firmware_device(&mut context, bus_id, &failed)
        .is_err());
    assert_eq!(context.registered_count(), 1);
    assert_eq!(kdevice::device_descs_snapshot().len(), 1);
}
