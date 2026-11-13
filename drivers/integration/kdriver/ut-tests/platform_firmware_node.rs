// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cross-boundary evidence that a discovery-stage firmware node reaches probe.
//!
//! These tests exercise the production `PlatformBackend::register_firmware_device`
//! wiring for the device-tree case (A1), and the lower-level
//! `device_desc_add`/`probe_device_desc` entry for a source-neutral fake backend
//! (A2/A5). They deliberately do not claim to cover `for_each_compatible`
//! discovery or candidate filtering.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU32, Ordering};

use driver_base::{DeviceKind, DriverError, DriverResult};
use kalloc_types::KBox;
use kdevice::{
    ActiveDeviceAdoption, BusId, DeviceDesc, DeviceDescId, DeviceDescSpec, DeviceDriver,
    DeviceIdentity, DeviceLocation, DeviceMatcher, DeviceObject, DiscoveryOrigin,
    FirmwareMatchSpec, MatchResult, PlatformIdentity, ResourceSet, adopt_active_device,
    device_desc_add, probe_device_desc, register_driver_object,
};
use khal::firmware::devices::{
    FirmwareDevice, FirmwareSource, FwError, FwIrqSpec, FwMmioResource, FwNode, FwNodeId,
    FwNodeOps, FwProperty, FwPropertyValue, FwReference, FwSource, MmioResource,
};
use kspin::SpinNoPreempt;
use ksync::static_lock;
use rs_fdtree::{DtBackend, LinuxFdt};
use unittest::{assert_eq, def_test};

use super::*;

// -- Shared observation -------------------------------------------------------

/// Result of reading one firmware node through the production handle.
///
/// A1 and A2 both feed their probed node through [`consume`], so the two
/// cases prove the same consumer code works for a real DT backend and an
/// unrelated source-neutral backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Consumed {
    node: FwNode<'static>,
    value: Result<Option<&'static str>, FwError>,
    target: Result<Option<&'static str>, FwError>,
}

static_lock! {
    static CONSUMED: SpinNoPreempt<Option<Consumed>> = SpinNoPreempt::new(None);
}

fn record_consumed(device: &DeviceObject) {
    *CONSUMED.lock() = consume(device.firmware_node());
}

fn taken_consumed() -> Consumed {
    (*CONSUMED.lock()).expect("probe did not observe a firmware node")
}

fn consume(node: Option<FwNode<'static>>) -> Option<Consumed> {
    let node = node?;
    Some(Consumed {
        node,
        value: read_string(node, "test-value"),
        target: read_reference_target(node),
    })
}

fn read_string(node: FwNode<'static>, name: &str) -> Result<Option<&'static str>, FwError> {
    match node.property(name)? {
        Some(property) => property.as_str().map(Some),
        None => Ok(None),
    }
}

fn read_reference_target(node: FwNode<'static>) -> Result<Option<&'static str>, FwError> {
    match node.reference("test-ref", 0, 0)? {
        Some(reference) => read_string(reference.node, "test-value"),
        None => Ok(None),
    }
}

/// Raw node id recorded by a probe; `u32::MAX` means "no node".
static PROBED_RAW: AtomicU32 = AtomicU32::new(u32::MAX);

// -- A1: real device-tree backend through the production helper --------------

/// Minimal flattened-device-tree writer.
///
/// Mirrors the one in `firmware_dma.rs`; kept local so the fixture here can
/// describe a device node with a phandle without widening that module's
/// riscv64-only fixture.
struct Dtb {
    structure: Vec<u8>,
    strings: Vec<u8>,
}

impl Dtb {
    fn new() -> Self {
        Self {
            structure: Vec::new(),
            strings: Vec::new(),
        }
    }

    fn word(&mut self, value: u32) {
        self.structure.extend_from_slice(&value.to_be_bytes());
    }

    fn align(&mut self) {
        while !self.structure.len().is_multiple_of(4) {
            self.structure.push(0);
        }
    }

    fn begin(&mut self, name: &str) {
        self.word(1); // FDT_BEGIN_NODE
        self.structure.extend_from_slice(name.as_bytes());
        self.structure.push(0);
        self.align();
        self.word(4); // FDT_NOP padding
    }

    fn property(&mut self, name: &str, bytes: &[u8]) {
        self.word(3); // FDT_PROP
        self.word(bytes.len() as u32);
        self.word(self.strings.len() as u32);
        self.strings.extend_from_slice(name.as_bytes());
        self.strings.push(0);
        self.structure.extend_from_slice(bytes);
        self.align();
    }

    fn end(&mut self) {
        self.word(2); // FDT_END_NODE
    }

    fn finish(mut self) -> Vec<u8> {
        self.word(9); // FDT_END
        let strings_offset = 56 + self.structure.len();
        let mut bytes = Vec::new();
        for value in [
            0xd00d_feed,
            (strings_offset + self.strings.len()) as u32,
            56,
            strings_offset as u32,
            40,
            17,
            16,
            0,
            self.strings.len() as u32,
            self.structure.len() as u32,
        ] {
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        bytes.extend_from_slice(&[0; 16]);
        bytes.extend_from_slice(&self.structure);
        bytes.extend_from_slice(&self.strings);
        bytes
    }
}

fn cells(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_be_bytes())
        .collect()
}

/// Synthetic tree:
///
/// ```text
/// / {
///     test-device@1000 {
///         compatible = "fwnode-test";
///         test-value = "dt-device";
///         test-ref = <&test-target>;
///     };
///     test-target {
///         phandle = <1>;
///         test-value = "dt-target";
///     };
/// };
/// ```
fn firmware_tree() -> Vec<u8> {
    let mut blob = Dtb::new();
    blob.begin("");
    blob.property("compatible", b"test,root\0");
    blob.property("#address-cells", &cells(&[1]));
    blob.property("#size-cells", &cells(&[1]));
    blob.begin("test-device@1000");
    blob.property("compatible", b"fwnode-test\0");
    blob.property("test-value", b"dt-device\0");
    blob.property("test-ref", &cells(&[1]));
    blob.end();
    blob.begin("test-target");
    blob.property("phandle", &cells(&[1]));
    blob.property("test-value", b"dt-target\0");
    blob.end();
    blob.end();
    blob.finish()
}

/// Builds one `FirmwareDevice` backed by the real [`DtBackend`].
///
/// The blob and backend must outlive the returned full `'static` handle.
/// Leaking them is a test-only way to simulate boot-static firmware storage;
/// production code must not leak or extend non-static firmware references.
fn dt_device() -> (FirmwareDevice, FwNode<'static>) {
    let bytes: &'static [u8] =
        KBox::leak(KBox::from_slice(&firmware_tree()).expect("fixture allocation failed"));
    let tree = LinuxFdt::new(bytes).expect("fixture is a valid device tree");
    let backend: &'static DtBackend<'static> =
        KBox::leak(KBox::new(DtBackend::new(tree)).expect("fixture allocation failed"));
    let node = backend
        .root()
        .expect("fixture has a root")
        .descendants()
        .find(|node| node.firmware_ids().any(|id| id == "fwnode-test"))
        .expect("fixture has a fwnode-test device");

    (
        FirmwareDevice {
            firmware_id: "fwnode-test",
            source: FirmwareSource::DeviceTree,
            node,
            mmio: Ok(None),
            irq: Ok(None),
        },
        node,
    )
}

/// Synthetic tree for the virtio-mmio discovery path:
///
/// ```text
/// / {
///     #address-cells = <1>;
///     #size-cells = <1>;
///     virtio@1000 {
///         compatible = "virtio,mmio";
///         reg = <0x1000 0x200>;
///     };
/// };
/// ```
fn virtio_mmio_tree() -> Vec<u8> {
    let mut blob = Dtb::new();
    blob.begin("");
    blob.property("#address-cells", &cells(&[1]));
    blob.property("#size-cells", &cells(&[1]));
    blob.begin("virtio@1000");
    blob.property("compatible", b"virtio,mmio\0");
    blob.property("reg", &cells(&[0x1000, 0x200]));
    blob.end();
    blob.end();
    blob.finish()
}

/// Builds a real-DT-backed `virtio,mmio` device whose normalized MMIO range is
/// read from the fixture's `reg` property.
fn dt_virtio_device() -> (FirmwareDevice, FwNode<'static>) {
    let bytes: &'static [u8] = KBox::leak(
        KBox::from_slice(&virtio_mmio_tree()).expect("fixture allocation failed"),
    );
    let tree = LinuxFdt::new(bytes).expect("fixture is a valid device tree");
    let backend: &'static DtBackend<'static> =
        KBox::leak(KBox::new(DtBackend::new(tree)).expect("fixture allocation failed"));
    let node = backend
        .root()
        .expect("fixture has a root")
        .descendants()
        .find(|node| {
            node.firmware_ids()
                .any(|id| id == VIRTIO_MMIO_COMPATIBLE)
        })
        .expect("fixture has a virtio,mmio device");
    let resource = node
        .mmio_resource(0)
        .expect("reg is readable")
        .expect("reg is declared");

    (
        FirmwareDevice {
            firmware_id: VIRTIO_MMIO_COMPATIBLE,
            source: FirmwareSource::DeviceTree,
            node,
            mmio: Ok(Some(MmioResource {
                base: usize::try_from(resource.base).expect("64-bit target"),
                size: usize::try_from(resource.size).expect("64-bit target"),
            })),
            irq: node.irq_spec(0),
        },
        node,
    )
}

static DT_SPEC: FirmwareMatchSpec = FirmwareMatchSpec {
    alias: "fwnode-test",
    dt_compatibles: &["fwnode-test"],
    acpi_ids: &[],
};

/// Driver used by A1/A4; it consumes the probed node through [`consume`].
struct DtConsumingDriver;

impl DeviceDriver for DtConsumingDriver {
    fn name(&self) -> &'static str {
        "fwnode-dt-consuming-test"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn DeviceMatcher {
        &DT_SPEC
    }

    fn probe_device(&self, device: Arc<DeviceObject>) -> DriverResult<()> {
        record_consumed(&device);
        Ok(())
    }
}

/// First matching driver: observe the node, then fail, so A4 can prove the
/// association survived into the failed attempt and the descriptor retry.
struct DtFailingDriver;

impl DeviceDriver for DtFailingDriver {
    fn name(&self) -> &'static str {
        "fwnode-dt-failing-test"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn DeviceMatcher {
        &DT_SPEC
    }

    fn probe_device(&self, device: Arc<DeviceObject>) -> DriverResult<()> {
        record_consumed(&device);
        Err(DriverError::Io)
    }
}

fn setup() -> (PlatformBackend, EnumerationContext, BusId) {
    let (_bus_type, bus) = kdevice::reset_and_setup_platform_bus_for_tests("fwnode-integration-bus");
    (
        PlatformBackend::new(),
        EnumerationContext::new(),
        bus.id(),
    )
}

#[def_test(serial)]
fn real_dt_node_reaches_probe_through_platform_registration() {
    *CONSUMED.lock() = None;
    let (device, node) = dt_device();
    let (mut backend, mut context, bus_id) = setup();
    register_driver_object(Arc::new(DtConsumingDriver));

    backend
        .register_firmware_device(&mut context, bus_id, &device)
        .unwrap();
    assert_eq!(context.registered_count(), 1);
    assert_eq!(context.probe_pending(), 1);

    // The probe reads firmware facts through the copied handle, including a
    // phandle reference resolved by the real backend.
    let consumed = taken_consumed();
    assert_eq!(consumed.node, node);
    assert_eq!(consumed.value, Ok(Some("dt-device")));
    assert_eq!(consumed.target, Ok(Some("dt-target")));

    let records = kdevice::device_records_snapshot();
    assert_eq!(records.len(), 1);
    let bound = kdevice::find_device(records[0].id).unwrap();
    assert_eq!(bound.firmware_node(), Some(node));
}

#[def_test(serial)]
fn real_dt_virtio_mmio_is_registered_as_a_description_only() {
    let (device, _node) = dt_virtio_device();
    let (mut backend, mut context, bus_id) = setup();

    backend
        .register_firmware_device(&mut context, bus_id, &device)
        .unwrap();
    assert_eq!(context.registered_count(), 1);

    let descs = kdevice::device_descs_snapshot();
    assert_eq!(descs.len(), 1);
    let desc = &descs[0];
    assert_eq!(
        desc.location(),
        DeviceLocation::Mmio {
            base: 0x1000,
            size: 0x200,
        }
    );
    assert_eq!(
        desc.identity(),
        DeviceIdentity::Platform(PlatformIdentity {
            alias: None,
            firmware_id: Some(VIRTIO_MMIO_COMPATIBLE),
        })
    );
    // Discovery describes the node; it does not read the device header. The
    // descriptor-level firmware-node accessor is device-core-internal; the
    // node association is covered by the probe-path test above.
    assert_eq!(desc.transport(), None);
    assert_eq!(desc.resources().len(), 1);

    // No driver in this test claims the descriptor, so probing it binds
    // nothing — the description alone is the discovery result. The stable
    // object registered with the descriptor remains, unbound (`Discovered`).
    assert_eq!(context.probe_pending(), 0);
    assert_eq!(context.unclaimed_count(), 1);
    let records = kdevice::device_records_snapshot();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].state, kdevice::DeviceState::Discovered);
    assert_eq!(records[0].driver_id, None);
}

// -- A2: source-neutral fake backend, same consumer --------------------------

const FAKE_TARGET_RAW: u32 = 0x00f0_0002;

/// A source-neutral backend: it reports [`FwSource::Unknown`] and is never
/// wrapped in a `FirmwareDevice`, so no DT/ACPI provenance is fabricated.
struct FakeOps {
    device: FwNodeId,
    target: FwNodeId,
}

impl FakeOps {
    fn known(&self, id: FwNodeId) -> Result<(), FwError> {
        if id == self.device || id == self.target {
            Ok(())
        } else {
            Err(FwError::InvalidNode)
        }
    }
}

impl FwNodeOps<'static> for FakeOps {
    fn name(&self, id: FwNodeId) -> Option<&'static str> {
        self.known(id).ok()?;
        (id == self.device).then_some("fake-device")
    }

    fn parent(&self, id: FwNodeId) -> Option<FwNodeId> {
        self.known(id).ok()?;
        None
    }

    fn first_child(&self, id: FwNodeId) -> Option<FwNodeId> {
        self.known(id).ok()?;
        None
    }

    fn next_sibling(&self, id: FwNodeId) -> Option<FwNodeId> {
        self.known(id).ok()?;
        None
    }

    fn is_available(&self, id: FwNodeId) -> Result<bool, FwError> {
        self.known(id)?;
        Ok(true)
    }

    fn is_interrupt_controller(&self, id: FwNodeId) -> Result<bool, FwError> {
        self.known(id)?;
        Ok(false)
    }

    fn firmware_id(&self, id: FwNodeId, index: usize) -> Option<&'static str> {
        self.known(id).ok()?;
        (id == self.device && index == 0).then_some("fake,fwnode-test")
    }

    fn property(&self, id: FwNodeId, name: &str) -> Result<Option<FwProperty<'static>>, FwError> {
        self.known(id)?;
        match (id == self.device, name) {
            (true, "test-value") => Ok(Some(FwProperty {
                name: "test-value",
                value: FwPropertyValue::Str("fake-device"),
            })),
            (false, "test-value") => Ok(Some(FwProperty {
                name: "test-value",
                value: FwPropertyValue::Str("fake-target"),
            })),
            (true, "bad-value") => Err(FwError::InvalidValue),
            _ => Ok(None),
        }
    }

    fn mmio_resource(
        &self,
        id: FwNodeId,
        _index: usize,
    ) -> Result<Option<FwMmioResource<'static>>, FwError> {
        self.known(id)?;
        Ok(None)
    }

    fn irq_spec(
        &'static self,
        id: FwNodeId,
        _index: usize,
    ) -> Result<Option<FwIrqSpec<'static>>, FwError> {
        self.known(id)?;
        Ok(None)
    }

    fn reference(
        &'static self,
        id: FwNodeId,
        _property: &str,
        _index: usize,
        _arg_cells: usize,
    ) -> Result<Option<FwReference<'static>>, FwError> {
        // This backend only proves that the shared consumer reaches the
        // backend's reference query without manufacturing a source; real
        // phandle resolution is covered by the DT fixture in A1.
        self.known(id)?;
        Ok(None)
    }
}

static ALIAS_FAKE: AliasMatcher = AliasMatcher("fwnode-fake");

/// Matches platform descriptors by alias, independent of discovery origin.
struct AliasMatcher(&'static str);

impl DeviceMatcher for AliasMatcher {
    fn matches(&self, desc: &DeviceDesc) -> MatchResult {
        match desc.identity() {
            DeviceIdentity::Platform(platform) if platform.alias == Some(self.0) => {
                MatchResult::Match
            }
            _ => MatchResult::NoMatch,
        }
    }
}

struct AliasConsumingDriver;

impl DeviceDriver for AliasConsumingDriver {
    fn name(&self) -> &'static str {
        "fwnode-alias-consuming-test"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn DeviceMatcher {
        &ALIAS_FAKE
    }

    fn probe_device(&self, device: Arc<DeviceObject>) -> DriverResult<()> {
        record_consumed(&device);
        Ok(())
    }
}

fn alias_desc(bus_id: BusId, id: u16, alias: &'static str, node: FwNode<'static>) -> DeviceDesc {
    device_desc_add(DeviceDescSpec {
        bus_id,
        parent: None,
        location: DeviceLocation::PlatformStatic { id },
        // The fake backend reports `FwSource::Unknown`; this test path never
        // goes through `FirmwareSource`, so no firmware origin is claimed for
        // it at the descriptor level either.
        origin: DiscoveryOrigin::PlatformStatic,
        identity: DeviceIdentity::Platform(PlatformIdentity {
            alias: Some(alias),
            firmware_id: None,
        }),
        transport: None,
        resources: ResourceSet::new(),
        dma: None,
        intx: None,
        firmware_node: Some(node),
    })
    .unwrap()
}

#[def_test(serial)]
fn fake_backend_node_reaches_the_same_consumer() {
    *CONSUMED.lock() = None;
    let (_device, dt_node) = dt_device();
    let raw = dt_node.id().raw();

    // Same backend-local id as the real DT node: identity is only meaningful
    // together with its backend, and the handles must not compare equal.
    let ops: &'static FakeOps = KBox::leak(
        KBox::new(FakeOps {
            device: FwNodeId::new(raw),
            target: FwNodeId::new(FAKE_TARGET_RAW),
        })
        .expect("fixture allocation failed"),
    );
    let fake_node = FwNode::new(ops, FwNodeId::new(raw));
    assert_eq!(fake_node.id(), dt_node.id());
    assert_ne!(fake_node, dt_node);
    assert_eq!(fake_node.source(), FwSource::Unknown);
    assert_eq!(dt_node.source(), FwSource::DeviceTree);

    let (_bus_type, bus) = kdevice::reset_and_setup_platform_bus_for_tests("fwnode-fake-bus");
    register_driver_object(Arc::new(AliasConsumingDriver));
    let desc = alias_desc(bus.id(), 1, "fwnode-fake", fake_node);
    assert_eq!(
        probe_device_desc(desc.id()).unwrap(),
        kdevice::ProbeOutcome::Activated
    );

    let consumed = taken_consumed();
    assert_eq!(consumed.node, fake_node);
    assert_eq!(consumed.value, Ok(Some("fake-device")));
    // The fake declines to resolve the reference; absence is `Ok(None)`, and
    // A1 already covers real phandle resolution.
    assert_eq!(consumed.target, Ok(None));

    let bound = kdevice::find_device(
        kdevice::device_records_snapshot()
            .first()
            .expect("device was published")
            .id,
    )
    .unwrap();
    assert_eq!(bound.firmware_node(), Some(fake_node));
}

// -- A4: failed probe retry and removal on the real path ---------------------

#[def_test(serial)]
fn failed_probe_retry_and_removal_keep_the_original_node() {
    *CONSUMED.lock() = None;
    let (device, node) = dt_device();
    let (mut backend, mut context, bus_id) = setup();
    register_driver_object(Arc::new(DtFailingDriver));

    backend
        .register_firmware_device(&mut context, bus_id, &device)
        .unwrap();
    assert_eq!(context.probe_pending(), 0);
    assert_eq!(context.unclaimed_count(), 1);

    // The failed attempt already received the correct association; its
    // binding was retired, and the stable object remains unbound so the
    // retry below reuses the same device id.
    assert_eq!(taken_consumed().node, node);
    let records = kdevice::device_records_snapshot();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].state, kdevice::DeviceState::Discovered);
    assert_eq!(records[0].driver_id, None);

    let desc_id: DeviceDescId = context.unclaimed()[0].id();
    *CONSUMED.lock() = None;

    // Registering a second matching driver retries the same descriptor with
    // its stored node; the descriptor id does not change.
    register_driver_object(Arc::new(DtConsumingDriver));

    let consumed = taken_consumed();
    assert_eq!(consumed.node, node);
    let records = kdevice::device_records_snapshot();
    assert_eq!(records.len(), 1);
    let bound = kdevice::find_device(records[0].id).unwrap();
    assert_eq!(bound.firmware_node(), Some(node));
    assert_eq!(
        kdevice::find_device_desc(desc_id)
            .expect("descriptor survives the retry")
            .id(),
        desc_id
    );

    // Unbinding ends the driver binding but not the stable object or the
    // shared, boot-static backend: the original discovery handle is still
    // readable after the binding is gone.
    let device_id = bound.id();
    kdevice::remove_device_managed(device_id).unwrap();
    drop(bound);
    let retained = kdevice::find_device(device_id).expect("unbind keeps the stable object");
    assert_eq!(retained.state(), kdevice::DeviceState::Discovered);
    assert_eq!(retained.driver_id(), None);
    drop(retained);
    assert_eq!(read_string(node, "test-value"), Ok(Some("dt-device")));
    assert_eq!(read_reference_target(node), Ok(Some("dt-target")));
}

// -- A5: absence, malformation and unknown handles stay distinct -------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Classification {
    missing: Result<Option<u32>, FwError>,
    malformed: Result<Option<u32>, FwError>,
}

static_lock! {
    static CLASSIFICATION: SpinNoPreempt<Option<Classification>> = SpinNoPreempt::new(None);
}
static_lock! {
    static UNKNOWN_HANDLE: SpinNoPreempt<Option<Result<Option<u32>, FwError>>> =
        SpinNoPreempt::new(None);
}

fn integer(node: FwNode<'static>, name: &str) -> Result<Option<u32>, FwError> {
    match node.property(name)? {
        Some(property) => property.as_u32().map(Some),
        None => Ok(None),
    }
}

static ALIAS_CLASSIFY: AliasMatcher = AliasMatcher("fwnode-classify");
static ALIAS_UNKNOWN: AliasMatcher = AliasMatcher("fwnode-unknown");

struct ClassifyingDriver;

impl DeviceDriver for ClassifyingDriver {
    fn name(&self) -> &'static str {
        "fwnode-classifying-test"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn DeviceMatcher {
        &ALIAS_CLASSIFY
    }

    fn probe_device(&self, device: Arc<DeviceObject>) -> DriverResult<()> {
        let node = device
            .firmware_node()
            .ok_or(DriverError::InvalidInput)?;
        *CLASSIFICATION.lock() = Some(Classification {
            missing: integer(node, "missing-value"),
            malformed: integer(node, "bad-value"),
        });
        Ok(())
    }
}

struct UnknownHandleDriver;

impl DeviceDriver for UnknownHandleDriver {
    fn name(&self) -> &'static str {
        "fwnode-unknown-handle-test"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn DeviceMatcher {
        &ALIAS_UNKNOWN
    }

    fn probe_device(&self, device: Arc<DeviceObject>) -> DriverResult<()> {
        let node = device
            .firmware_node()
            .ok_or(DriverError::InvalidInput)?;
        *UNKNOWN_HANDLE.lock() = Some(node.property("test-value").and_then(|property| {
            match property {
                Some(property) => property.as_u32().map(Some),
                None => Ok(None),
            }
        }));
        Ok(())
    }
}

fn fake_ops(device: FwNodeId) -> &'static FakeOps {
    KBox::leak(
        KBox::new(FakeOps {
            device,
            target: FwNodeId::new(FAKE_TARGET_RAW),
        })
        .expect("fixture allocation failed"),
    )
}

#[def_test(serial)]
fn missing_and_malformed_properties_stay_distinct() {
    *CLASSIFICATION.lock() = None;
    let ops = fake_ops(FwNodeId::new(1));
    let node = FwNode::new(ops, FwNodeId::new(1));

    let (_bus_type, bus) = kdevice::reset_and_setup_platform_bus_for_tests("fwnode-classify-bus");
    register_driver_object(Arc::new(ClassifyingDriver));
    let desc = alias_desc(bus.id(), 1, "fwnode-classify", node);
    assert_eq!(
        probe_device_desc(desc.id()).unwrap(),
        kdevice::ProbeOutcome::Activated
    );

    let classification = (*CLASSIFICATION.lock()).expect("probe did not classify the node");
    // Absence is not an error and corruption is not absence.
    assert_eq!(classification.missing, Ok(None));
    assert_eq!(classification.malformed, Err(FwError::InvalidValue));
}

#[def_test(serial)]
fn unknown_node_handle_is_invalid_node() {
    *UNKNOWN_HANDLE.lock() = None;
    let ops = fake_ops(FwNodeId::new(1));
    let unknown = FwNode::new(ops, FwNodeId::new(0xdead_beef));
    assert_eq!(
        unknown.property("test-value"),
        Err(FwError::InvalidNode)
    );

    let (_bus_type, bus) = kdevice::reset_and_setup_platform_bus_for_tests("fwnode-unknown-bus");
    register_driver_object(Arc::new(UnknownHandleDriver));
    let desc = alias_desc(bus.id(), 1, "fwnode-unknown", unknown);
    assert_eq!(
        probe_device_desc(desc.id()).unwrap(),
        kdevice::ProbeOutcome::Activated
    );

    assert_eq!(
        (*UNKNOWN_HANDLE.lock()).expect("probe did not classify the node"),
        Err(FwError::InvalidNode)
    );
}

// -- A6: no-node adoption and platform-static paths --------------------------

static ALIAS_STATIC: AliasMatcher = AliasMatcher("fwnode-static");

struct StaticRecordingDriver;

impl DeviceDriver for StaticRecordingDriver {
    fn name(&self) -> &'static str {
        "fwnode-static-recording-test"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn DeviceMatcher {
        &ALIAS_STATIC
    }

    fn probe_device(&self, device: Arc<DeviceObject>) -> DriverResult<()> {
        PROBED_RAW.store(
            device
                .firmware_node()
                .map_or(u32::MAX, |node| node.id().raw()),
            Ordering::SeqCst,
        );
        Ok(())
    }
}

#[def_test(serial)]
fn platform_static_device_registers_without_a_firmware_node() {
    PROBED_RAW.store(0, Ordering::SeqCst);
    let (_backend, mut context, bus_id) = setup();
    register_driver_object(Arc::new(StaticRecordingDriver));

    PlatformBackend::register_static_device(
        &mut context,
        bus_id,
        DeviceLocation::PlatformStatic { id: 1 },
        DeviceIdentity::Platform(PlatformIdentity {
            alias: Some("fwnode-static"),
            firmware_id: None,
        }),
        ResourceSet::new(),
    )
    .unwrap();

    assert_eq!(context.probe_pending(), 1);
    assert_eq!(PROBED_RAW.load(Ordering::SeqCst), u32::MAX);
    assert_eq!(kdevice::device_records_snapshot().len(), 1);
}

#[def_test(serial)]
fn adopted_device_has_no_firmware_node() {
    let (_bus_type, bus) = kdevice::reset_and_setup_platform_bus_for_tests("fwnode-adopt-bus");
    let driver = register_driver_object(Arc::new(StaticRecordingDriver));

    let device = adopt_active_device(ActiveDeviceAdoption {
        bus_id: bus.id(),
        parent: None,
        location: DeviceLocation::PlatformStatic { id: 1 },
        origin: DiscoveryOrigin::PlatformStatic,
        identity: DeviceIdentity::Platform(PlatformIdentity {
            alias: Some("fwnode-static"),
            firmware_id: None,
        }),
        transport: None,
        resources: ResourceSet::new(),
        dma: None,
        driver,
    })
    .unwrap();

    // Adoption is a boot/runtime handoff, not firmware discovery: the driver
    // is bound without probing, and the published object carries no node.
    assert_eq!(device.firmware_node(), None);
}
