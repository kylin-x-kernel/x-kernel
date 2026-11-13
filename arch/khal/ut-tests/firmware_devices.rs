// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Normalization of firmware MMIO declarations into [`FirmwareDevice`].
//!
//! Valid, absent and unreadable declarations must stay distinguishable, and
//! `u64 -> usize` conversion must be exact rather than truncating.

use alloc::vec::Vec;

use kalloc_types::KBox;
use rs_fdtree::{DtBackend, LinuxFdt};
use unittest::{assert_eq, def_test};

use super::*;

const NODE_RAW: u32 = 1;

/// Minimal flattened-device-tree writer.
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

/// Synthetic tree with one `virtio,mmio` node:
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
fn virtio_tree() -> Vec<u8> {
    let mut blob = Dtb::new();
    blob.begin("");
    blob.property("#address-cells", &0x1u32.to_be_bytes());
    blob.property("#size-cells", &0x1u32.to_be_bytes());
    blob.begin("virtio@1000");
    blob.property("compatible", b"virtio,mmio\0");
    let mut reg = Vec::new();
    reg.extend_from_slice(&0x1000u32.to_be_bytes());
    reg.extend_from_slice(&0x200u32.to_be_bytes());
    blob.property("reg", &reg);
    blob.end();
    blob.end();
    blob.finish()
}

/// Finds the real DT node carrying `compatible`.
///
/// The blob and backend are leaked to simulate boot-static firmware storage;
/// the returned handle is only valid because of that leak.
fn dt_node() -> FwNode<'static> {
    let bytes: &'static [u8] = virtio_tree().leak();
    let tree = LinuxFdt::new(bytes).expect("fixture is a valid device tree");
    let backend: &'static DtBackend<'static> = KBox::leak(
        KBox::new_nowait(DtBackend::new(tree)).expect("firmware backend allocation failed"),
    );
    backend
        .root()
        .expect("fixture has a root")
        .descendants()
        .find(|node| node.firmware_ids().any(|id| id == "virtio,mmio"))
        .expect("fixture has a virtio,mmio node")
}

/// Fake backend returning one configured MMIO result.
struct FakeOps {
    mmio: Result<Option<FwMmioResource<'static>>, FwError>,
}

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
        self.mmio
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

fn fake_device(mmio: Result<Option<FwMmioResource<'static>>, FwError>) -> FirmwareDevice {
    let ops: &'static FakeOps =
        KBox::leak(KBox::new_nowait(FakeOps { mmio }).expect("firmware ops allocation failed"));
    firmware_device_from_node(FwNode::new(ops, FwNodeId::new(NODE_RAW)), "test,platform")
}

#[def_test]
fn real_dt_virtio_node_normalizes_its_reg_range() {
    let node = dt_node();
    let device = firmware_device_from_node(node, "virtio,mmio");

    assert_eq!(device.node, node);
    assert_eq!(device.firmware_id, "virtio,mmio");
    assert_eq!(
        device.mmio,
        Ok(Some(MmioResource {
            base: 0x1000,
            size: 0x200,
        }))
    );
    assert_eq!(device.irq, Ok(None));
}

#[def_test]
fn valid_fake_range_converts_exactly() {
    // All supported targets are 64-bit, so a real overflow cannot be produced
    // at runtime. The test asserts exact high-address conversion; the code
    // keeps `usize::try_from` rather than a truncating cast.
    let device = fake_device(Ok(Some(FwMmioResource {
        base: 0x1_0000_0000,
        size: 0x4000,
        name: Some("mmio0"),
    })));
    assert_eq!(
        device.mmio,
        Ok(Some(MmioResource {
            base: 0x1_0000_0000,
            size: 0x4000,
        }))
    );
}

#[def_test]
fn absent_first_mmio_is_not_an_error() {
    let device = fake_device(Ok(None));
    assert_eq!(device.mmio, Ok(None));
}

#[def_test]
fn unreadable_mmio_preserves_the_backend_error() {
    for error in [
        FwError::Unsupported,
        FwError::InvalidValue,
        FwError::InvalidNode,
    ] {
        let device = fake_device(Err(error));
        assert_eq!(device.mmio, Err(error));
    }
}
