// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(target_arch = "riscv64")]
use alloc::vec::Vec;

use khal::firmware::devices::{
    FirmwareDevice, FirmwareSource, FwDmaError, FwDmaWindow, FwError, FwIrqSpec, FwMmioResource,
    FwNode, FwNodeId, FwNodeOps, FwProperty, FwReference, FwSource,
};
#[cfg(target_arch = "riscv64")]
use kalloc_types::KBox;
#[cfg(target_arch = "riscv64")]
use rs_fdtree::{DtBackend, LinuxFdt};
use unittest::{assert_eq, def_test};

use super::*;

/// One synthetic firmware node.
///
/// The DMA window and effective coherency are already resolved by the firmware
/// backend; this fake deliberately contains no Device Tree walking so the
/// tests cover only this module's composition policy.
struct Node {
    parent: Option<u32>,
    compatibles: &'static [&'static str],
    window: Result<Option<FwDmaWindow>, FwDmaError>,
    coherent: Result<Option<bool>, FwError>,
    unavailable: bool,
}

const fn node(
    parent: Option<u32>,
    compatibles: &'static [&'static str],
    window: Result<Option<FwDmaWindow>, FwDmaError>,
    coherent: Result<Option<bool>, FwError>,
) -> Node {
    Node {
        parent,
        compatibles,
        window,
        coherent,
        unavailable: false,
    }
}

struct FakeOps {
    nodes: &'static [Node],
}

impl FakeOps {
    fn lookup(&self, id: FwNodeId) -> Result<&Node, FwError> {
        self.nodes.get(id.raw() as usize).ok_or(FwError::InvalidNode)
    }
}

impl<'a> FwNodeOps<'a> for FakeOps {
    fn source(&self) -> FwSource {
        FwSource::DeviceTree
    }

    fn name(&self, id: FwNodeId) -> Option<&'a str> {
        self.lookup(id).ok().map(|_| "test-node")
    }

    fn parent(&self, id: FwNodeId) -> Option<FwNodeId> {
        self.lookup(id)
            .ok()
            .and_then(|node| node.parent)
            .map(FwNodeId::new)
    }

    fn first_child(&self, _id: FwNodeId) -> Option<FwNodeId> {
        None
    }

    fn next_sibling(&self, _id: FwNodeId) -> Option<FwNodeId> {
        None
    }

    fn is_available(&self, id: FwNodeId) -> Result<bool, FwError> {
        self.lookup(id).map(|node| !node.unavailable)
    }

    fn is_interrupt_controller(&self, id: FwNodeId) -> Result<bool, FwError> {
        self.lookup(id).map(|_| false)
    }

    fn firmware_id(&self, id: FwNodeId, index: usize) -> Option<&'a str> {
        self.lookup(id)
            .ok()
            .and_then(|node| node.compatibles.get(index))
            .copied()
    }

    fn property(&self, _id: FwNodeId, _name: &str) -> Result<Option<FwProperty<'a>>, FwError> {
        Ok(None)
    }

    fn mmio_resource(
        &self,
        _id: FwNodeId,
        _index: usize,
    ) -> Result<Option<FwMmioResource<'a>>, FwError> {
        Ok(None)
    }

    fn dma_window(&self, id: FwNodeId) -> Result<Option<FwDmaWindow>, FwDmaError> {
        self.lookup(id)
            .map_err(FwDmaError::Firmware)?
            .window
    }

    fn dma_effective_coherency(&self, id: FwNodeId) -> Result<Option<bool>, FwError> {
        self.lookup(id)?.coherent
    }

    fn irq_spec(&'a self, _id: FwNodeId, _index: usize) -> Result<Option<FwIrqSpec<'a>>, FwError> {
        Ok(None)
    }

    fn reference(
        &'a self,
        _id: FwNodeId,
        _property: &str,
        _index: usize,
        _arg_cells: usize,
    ) -> Result<Option<FwReference<'a>>, FwError> {
        Ok(None)
    }
}

fn device(ops: &'static FakeOps, id: u32) -> FirmwareDevice {
    FirmwareDevice {
        firmware_id: "test,dma",
        source: FirmwareSource::DeviceTree,
        node: FwNode::new(ops, FwNodeId::new(id)),
        mmio: None,
        irq: Ok(None),
    }
}

const WINDOW: FwDmaWindow = FwDmaWindow {
    bus_addr: 0x1000,
    cpu_addr: 0x8000,
    size: 0x1000,
};

static LOCAL_NODES: [Node; 2] = [
    node(None, &["test,root"], Ok(None), Ok(None)),
    node(Some(0), &["test,dma"], Ok(Some(WINDOW)), Ok(Some(true))),
];

static LOCAL_OPS: FakeOps = FakeOps {
    nodes: &LOCAL_NODES,
};

// -- One resolved window ------------------------------------------------------

#[def_test]
fn explicit_window_keeps_mask_and_coherency_independent() {
    let info = from_firmware(&device(&LOCAL_OPS, 1), 0x1fff).unwrap();
    assert_eq!(info.mask(), 0x1fff);
    assert_eq!(info.coherent(), true);
    assert_eq!(info.translate_range(0x8000, 0x1000), Some(0x1000));
    assert_eq!(info.translate_range(0x8000, 0x1001), None);
    assert_eq!(info.translate(0x9000), None);
    assert_eq!(info.translate(0x7fff), None);
}

#[def_test]
fn missing_coherency_is_not_defaulted() {
    static NODES: [Node; 2] = [
        node(None, &["test,root"], Ok(None), Ok(None)),
        node(Some(0), &["test,dma"], Ok(Some(WINDOW)), Ok(None)),
    ];
    static OPS: FakeOps = FakeOps { nodes: &NODES };

    assert_eq!(
        from_firmware(&device(&OPS, 1), u64::MAX),
        Err(DmaConfigurationError::MissingCoherency)
    );
}

#[def_test]
fn missing_window_is_not_identity() {
    static NODES: [Node; 2] = [
        node(None, &["test,root"], Ok(None), Ok(None)),
        node(Some(0), &["test,dma"], Ok(None), Ok(Some(true))),
    ];
    static OPS: FakeOps = FakeOps { nodes: &NODES };

    assert_eq!(
        from_firmware(&device(&OPS, 1), u64::MAX),
        Err(DmaConfigurationError::MissingWindow)
    );
}

#[def_test]
fn invalid_device_masks_are_rejected_before_firmware_queries() {
    for mask in [0x17ff, 0x1000, 0x8000_0000_0000_0001] {
        assert_eq!(
            from_firmware(&device(&LOCAL_OPS, 1), mask),
            Err(DmaConfigurationError::InvalidMask)
        );
    }
}

#[def_test]
fn firmware_window_errors_keep_their_meaning() {
    for (firmware, expected) in [
        (FwDmaError::Firmware(FwError::Unsupported), DmaConfigurationError::Firmware(FwError::Unsupported)),
        (FwDmaError::Firmware(FwError::InvalidValue), DmaConfigurationError::Firmware(FwError::InvalidValue)),
        (FwDmaError::MultiWindow, DmaConfigurationError::UnsupportedMultiWindow),
        (FwDmaError::UnsupportedParent, DmaConfigurationError::UnsupportedDmaParent),
        (FwDmaError::Iommu, DmaConfigurationError::UnsupportedIommu),
        (FwDmaError::UnsupportedTranslation, DmaConfigurationError::UnsupportedTranslation),
        (FwDmaError::InvalidWindow, DmaConfigurationError::InvalidWindow),
    ] {
        assert_eq!(map_dma_window_error(firmware), expected);
    }
}

#[def_test]
fn unsupported_firmware_topology_propagates_through_composition() {
    static NODES: [Node; 2] = [
        node(None, &["test,root"], Ok(None), Ok(None)),
        node(Some(0), &["test,dma"], Err(FwDmaError::Iommu), Ok(Some(true))),
    ];
    static OPS: FakeOps = FakeOps { nodes: &NODES };

    assert_eq!(
        from_firmware(&device(&OPS, 1), u64::MAX),
        Err(DmaConfigurationError::UnsupportedIommu)
    );
}

#[def_test]
fn invalid_composed_windows_are_rejected() {
    for window in [
        FwDmaWindow {
            bus_addr: 0,
            cpu_addr: 0,
            size: 0,
        },
        FwDmaWindow {
            bus_addr: 0,
            cpu_addr: u64::MAX,
            size: 2,
        },
    ] {
        assert_eq!(
            build(u64::MAX, window, true),
            Err(DmaConfigurationError::InvalidWindow)
        );
    }
}

#[def_test]
fn coherency_errors_are_preserved_as_firmware_errors() {
    static NODES: [Node; 2] = [
        node(None, &["test,root"], Ok(None), Ok(None)),
        node(
            Some(0),
            &["test,dma"],
            Ok(Some(WINDOW)),
            Err(FwError::InvalidValue),
        ),
    ];
    static OPS: FakeOps = FakeOps { nodes: &NODES };

    assert_eq!(
        from_firmware(&device(&OPS, 1), u64::MAX),
        Err(DmaConfigurationError::Firmware(FwError::InvalidValue))
    );
}

// -- Platform policy ----------------------------------------------------------

#[def_test]
fn platform_policy_never_overrides_explicit_firmware_facts() {
    let info = from_firmware_with_qemu_virt_mmio_policy(&device(&LOCAL_OPS, 1), 0x1fff).unwrap();
    assert_eq!(info.translate_range(0x8000, 0x1000), Some(0x1000));
    assert_eq!(info.mask(), 0x1fff);

    assert_eq!(
        from_firmware_with_qemu_virt_mmio_policy(&device(&LOCAL_OPS, 1), 0x17ff),
        Err(DmaConfigurationError::InvalidMask)
    );
}

#[def_test]
fn platform_policy_requires_a_scoped_available_node() {
    static NODES: [Node; 2] = [
        node(None, &["test,root"], Ok(None), Ok(None)),
        node(Some(0), &["test,device"], Ok(None), Ok(Some(true))),
    ];
    static OPS: FakeOps = FakeOps { nodes: &NODES };

    assert_eq!(
        from_firmware_with_qemu_virt_mmio_policy(&device(&OPS, 1), u64::MAX),
        Err(DmaConfigurationError::UnsupportedPlatformPolicy)
    );
}

#[def_test]
fn platform_policy_does_not_erase_firmware_errors() {
    static NODES: [Node; 2] = [
        node(None, &["riscv-virtio"], Ok(None), Ok(None)),
        node(
            Some(0),
            &["virtio,mmio"],
            Err(FwDmaError::Firmware(FwError::InvalidValue)),
            Ok(None),
        ),
    ];
    static OPS: FakeOps = FakeOps { nodes: &NODES };

    assert_eq!(
        from_firmware_with_qemu_virt_mmio_policy(&device(&OPS, 1), u64::MAX),
        Err(DmaConfigurationError::Firmware(FwError::InvalidValue))
    );
}

#[def_test]
fn platform_policy_only_crosses_simple_bus_containers() {
    static NODES: [Node; 3] = [
        node(None, &["riscv-virtio"], Ok(None), Ok(None)),
        node(Some(0), &["test,bus"], Ok(None), Ok(None)),
        node(Some(1), &["virtio,mmio"], Ok(None), Ok(None)),
    ];
    static OPS: FakeOps = FakeOps { nodes: &NODES };

    assert_eq!(
        from_firmware_with_qemu_virt_mmio_policy(&device(&OPS, 2), u64::MAX),
        Err(DmaConfigurationError::UnsupportedPlatformPolicy)
    );
}

#[def_test]
fn platform_policy_rejects_unavailable_containers() {
    static NODES: [Node; 3] = [
        node(None, &["riscv-virtio"], Ok(None), Ok(None)),
        Node {
            unavailable: true,
            ..node(Some(0), &["qemu,platform", "simple-bus"], Ok(None), Ok(None))
        },
        node(Some(1), &["virtio,mmio"], Ok(None), Ok(None)),
    ];
    static OPS: FakeOps = FakeOps { nodes: &NODES };

    assert_eq!(
        from_firmware_with_qemu_virt_mmio_policy(&device(&OPS, 2), u64::MAX),
        Err(DmaConfigurationError::UnsupportedPlatformPolicy)
    );
}

#[def_test]
fn platform_policy_requires_the_recognized_root_at_the_top() {
    static NODES: [Node; 3] = [
        node(None, &["test,root"], Ok(None), Ok(None)),
        node(Some(0), &["riscv-virtio"], Ok(None), Ok(None)),
        node(Some(1), &["virtio,mmio"], Ok(None), Ok(None)),
    ];
    static OPS: FakeOps = FakeOps { nodes: &NODES };

    assert_eq!(
        from_firmware_with_qemu_virt_mmio_policy(&device(&OPS, 2), u64::MAX),
        Err(DmaConfigurationError::UnsupportedPlatformPolicy)
    );
}

/// RISC-V places virtio-mmio under `soc/simple-bus`; Linux then applies
/// `ARCH_DMA_DEFAULT_COHERENT` and an identity DMA mapping for the machine.
#[cfg(target_arch = "riscv64")]
#[def_test]
fn platform_policy_resolves_qemu_riscv_simple_bus_path() {
    static NODES: [Node; 3] = [
        node(None, &["riscv-virtio"], Ok(None), Ok(None)),
        node(Some(0), &["qemu,platform", "simple-bus"], Ok(None), Ok(None)),
        node(Some(1), &["virtio,mmio"], Ok(None), Ok(None)),
    ];
    static OPS: FakeOps = FakeOps { nodes: &NODES };

    // The RAM window starts above the test mask used elsewhere; use an
    // unrestricted device mask so this exercises the platform window only.
    let info = from_firmware_with_qemu_virt_mmio_policy(&device(&OPS, 2), u64::MAX).unwrap();
    let [(start, size)] = khal::mem::boot_ram_regions().expect("test RAM is described") else {
        panic!("test environment must describe exactly one RAM region");
    };
    assert_eq!(info.mask(), u64::MAX);
    assert_eq!(info.coherent(), true);
    assert_eq!(info.window().is_identity(), true);
    assert_eq!(info.window().cpu_start(), *start as u64);
    assert_eq!(info.window().bus_start(), *start as u64);
    assert_eq!(
        info.translate_range(*start as u64, *size as u64),
        Some(*start as u64)
    );
}

// -- Real device-tree backend -------------------------------------------------

/// Minimal flattened-device-tree writer for the real-backend tests.
#[cfg(target_arch = "riscv64")]
struct Dtb {
    structure: Vec<u8>,
    strings: Vec<u8>,
}

#[cfg(target_arch = "riscv64")]
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
        self.word(1);
        self.structure.extend_from_slice(name.as_bytes());
        self.structure.push(0);
        self.align();
        self.word(4);
    }

    fn property(&mut self, name: &str, bytes: &[u8]) {
        self.word(3);
        self.word(bytes.len() as u32);
        self.word(self.strings.len() as u32);
        self.strings.extend_from_slice(name.as_bytes());
        self.strings.push(0);
        self.structure.extend_from_slice(bytes);
        self.align();
    }

    fn end(&mut self) {
        self.word(2);
    }

    fn finish(mut self) -> Vec<u8> {
        self.word(9);
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

#[cfg(target_arch = "riscv64")]
fn cells(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_be_bytes())
        .collect()
}

/// Builds a QEMU RISC-V `virt`-shaped tree: a `riscv-virtio` root, a
/// `simple-bus` `soc`, and one `virtio,mmio` device.
#[cfg(target_arch = "riscv64")]
fn virt_tree(dma_ranges: Option<&[u8]>) -> Vec<u8> {
    let mut blob = Dtb::new();
    blob.begin("");
    blob.property("compatible", b"riscv-virtio\0");
    blob.property("#address-cells", &cells(&[2]));
    blob.property("#size-cells", &cells(&[2]));
    blob.begin("soc");
    blob.property("compatible", b"simple-bus\0");
    blob.property("#address-cells", &cells(&[2]));
    blob.property("#size-cells", &cells(&[2]));
    if let Some(ranges) = dma_ranges {
        blob.property("dma-ranges", ranges);
    }
    blob.begin("virtio_mmio@10001000");
    blob.property("compatible", b"virtio,mmio\0");
    blob.end();
    blob.end();
    blob.end();
    blob.finish()
}

/// Feeds a synthetic device tree through the real [`DtBackend`] instead of the
/// neutral fake above, so backend absence semantics are part of the fixture.
///
/// The blob and backend must outlive the returned `FirmwareDevice` as
/// `'static` data. Both allocations are intentionally leaked for the lifetime
/// of the test binary.
#[cfg(target_arch = "riscv64")]
fn dt_device(bytes: Vec<u8>) -> FirmwareDevice {
    let bytes: &'static [u8] =
        KBox::leak(KBox::from_slice(&bytes).expect("fixture allocation failed"));
    let tree = LinuxFdt::new(bytes).expect("fixture is a valid device tree");
    let backend: &'static DtBackend<'static> =
        KBox::leak(KBox::new(DtBackend::new(tree)).expect("fixture allocation failed"));
    let node = backend
        .root()
        .expect("fixture has a root")
        .descendants()
        .find(|node| node.firmware_ids().any(|id| id == "virtio,mmio"))
        .expect("fixture has a virtio-mmio device");
    FirmwareDevice {
        firmware_id: "virtio,mmio",
        source: FirmwareSource::DeviceTree,
        node,
        mmio: None,
        irq: Ok(None),
    }
}

/// The real backend reports "no `dma-ranges` on the DMA-parent path" as
/// `Ok(None)`, so the platform policy must be reachable from it — the fake's
/// `Ok(None)` alone could not prove this.
#[cfg(target_arch = "riscv64")]
#[def_test]
fn real_dt_absence_reaches_the_qemu_platform_policy() {
    let device = dt_device(virt_tree(None));

    assert_eq!(device.node.dma_window(), Ok(None));
    assert_eq!(
        from_firmware(&device, u64::MAX),
        Err(DmaConfigurationError::MissingWindow)
    );

    let info = from_firmware_with_qemu_virt_mmio_policy(&device, u64::MAX).unwrap();
    let [(start, size)] = khal::mem::boot_ram_regions().expect("test RAM is described") else {
        panic!("test environment must describe exactly one RAM region");
    };
    assert_eq!(info.mask(), u64::MAX);
    assert_eq!(info.coherent(), true);
    assert_eq!(info.window().is_identity(), true);
    assert_eq!(info.window().cpu_start(), *start as u64);
    assert_eq!(info.window().bus_start(), *start as u64);
    assert_eq!(
        info.translate_range(*start as u64, *size as u64),
        Some(*start as u64)
    );
}

/// A malformed declaration is still an error at the neutral boundary and must
/// not be converted into absence by the platform-policy path.
#[cfg(target_arch = "riscv64")]
#[def_test]
fn real_dt_corruption_does_not_reach_the_platform_policy() {
    // A nonempty `dma-ranges` cannot form one complete tuple on `soc`.
    let device = dt_device(virt_tree(Some(&cells(&[0, 0x1000]))));

    assert_eq!(
        device.node.dma_window(),
        Err(FwDmaError::Firmware(FwError::InvalidValue))
    );
    assert_eq!(
        from_firmware_with_qemu_virt_mmio_policy(&device, u64::MAX),
        Err(DmaConfigurationError::Firmware(FwError::InvalidValue))
    );
}
