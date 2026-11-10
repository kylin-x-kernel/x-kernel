// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![cfg(feature = "fwnode")]

mod support;

use fwnode::{FwDmaError, FwDmaRange, FwDmaRanges, FwDmaWindow, FwError, FwNode};
use rs_fdtree::{DtBackend, LinuxFdt};
use support::Dtb;

fn cells(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_be_bytes())
        .collect()
}

fn node<'a>(root: FwNode<'a>, name: &str) -> FwNode<'a> {
    root.descendants()
        .find(|node| node.name() == Some(name))
        .unwrap()
}

fn irq_fixture() -> Vec<u8> {
    let mut blob = Dtb::new();
    blob.begin("");
    blob.begin("intc");
    blob.property("compatible", b"arm,gic-v3\0");
    blob.property("interrupt-controller", &[]);
    blob.property("#interrupt-cells", &cells(&[3]));
    blob.property("phandle", &cells(&[1]));
    blob.end();
    blob.begin("intc2");
    blob.property("compatible", b"sifive,plic-1.0.0\0");
    blob.property("interrupt-controller", &[]);
    blob.property("#interrupt-cells", &cells(&[1]));
    blob.property("phandle", &cells(&[2]));
    blob.end();
    blob.begin("intc3");
    blob.property("compatible", b"test,bad-controller\0");
    blob.property("interrupt-controller", &[]);
    blob.property("phandle", &cells(&[3]));
    blob.end();
    blob.begin("bus");
    blob.property("interrupt-parent", &cells(&[1]));
    blob.begin("uart");
    blob.property("compatible", b"arm,pl011\0");
    blob.property("interrupts", &cells(&[0, 5, 4]));
    blob.property("refs", &cells(&[1, 3, 2, 9]));
    blob.end();
    blob.end();
    blob.begin("device");
    blob.property("interrupts-extended", &cells(&[1, 0, 6, 4, 2, 7]));
    blob.end();
    blob.begin("bare");
    blob.end();
    blob.begin("bad-parent");
    blob.property("interrupts-extended", &cells(&[9, 0, 5, 4]));
    blob.end();
    blob.begin("missing-cells");
    blob.property("interrupt-parent", &cells(&[3]));
    blob.property("interrupts", &cells(&[5]));
    blob.end();
    blob.begin("no-parent");
    blob.property("interrupts", &cells(&[5, 0, 4]));
    blob.end();
    blob.begin("short");
    blob.property("interrupt-parent", &cells(&[1]));
    blob.property("interrupts", &cells(&[0, 5]));
    blob.end();
    blob.end();
    blob.finish()
}

fn fixture() -> Vec<u8> {
    let mut blob = Dtb::new();
    blob.begin("");
    blob.property("compatible", b"test,root\0");
    blob.begin("gic");
    blob.property("compatible", b"arm,gic-v3\0");
    blob.property("interrupt-controller", &[]);
    blob.property("#interrupt-cells", &cells(&[3]));
    blob.property("status", b"okay\0");
    blob.end();
    blob.begin("second");
    blob.end();
    blob.end();
    blob.finish()
}

#[test]
fn exposes_tree_structure_and_properties() {
    let bytes = fixture();
    let tree = LinuxFdt::new(&bytes).unwrap();
    let backend = DtBackend::new(tree);
    let root = backend.root().unwrap();

    assert_eq!(root.name(), Some("/"));
    assert_eq!(
        root.property("compatible").unwrap().unwrap().as_str(),
        Ok("test,root")
    );
    assert_eq!(root.firmware_ids().collect::<Vec<_>>(), ["test,root"]);

    let children: Vec<_> = root.children().map(|node| node.name().unwrap()).collect();
    assert_eq!(children, ["gic", "second"]);

    let descendants: Vec<_> = root
        .descendants()
        .map(|node| node.name().unwrap())
        .collect();
    assert_eq!(descendants, ["/", "gic", "second"]);

    let gic = root.first_child().unwrap();
    assert_eq!(gic.name(), Some("gic"));
    assert_eq!(gic.parent(), Some(root));
    assert_eq!(gic.next_sibling().unwrap().name(), Some("second"));
    assert!(gic.next_sibling().unwrap().next_sibling().is_none());
    assert_eq!(gic.is_available(), Ok(true));
    assert_eq!(gic.is_interrupt_controller(), Ok(true));
    assert_eq!(root.is_interrupt_controller(), Ok(false));
    assert_eq!(
        gic.property("#interrupt-cells").unwrap().unwrap().as_u32(),
        Ok(3)
    );
    assert_eq!(
        gic.property("interrupt-controller")
            .unwrap()
            .unwrap()
            .as_bool(),
        Ok(true)
    );
}

#[test]
fn resolves_inherited_interrupts_against_the_controller() {
    let bytes = irq_fixture();
    let tree = LinuxFdt::new(&bytes).unwrap();
    let backend = DtBackend::new(tree);
    let root = backend.root().unwrap();
    let uart = node(root, "uart");
    let intc = node(root, "intc");

    let spec = uart.irq_spec(0).unwrap().unwrap();
    assert_eq!(spec.controller, intc);
    assert_eq!(spec.params.as_slice(), [0, 5, 4]);
    assert_eq!(uart.irq_spec(1), Ok(None));
}

#[test]
fn enumerates_interrupts_extended_entries() {
    let bytes = irq_fixture();
    let tree = LinuxFdt::new(&bytes).unwrap();
    let backend = DtBackend::new(tree);
    let root = backend.root().unwrap();
    let device = node(root, "device");

    let first = device.irq_spec(0).unwrap().unwrap();
    assert_eq!(first.controller, node(root, "intc"));
    assert_eq!(first.params.as_slice(), [0, 6, 4]);

    let second = device.irq_spec(1).unwrap().unwrap();
    assert_eq!(second.controller, node(root, "intc2"));
    assert_eq!(second.params.as_slice(), [7]);

    assert_eq!(device.irq_spec(2), Ok(None));
    assert_eq!(node(root, "bare").irq_spec(0), Ok(None));
}

#[test]
fn resolves_references_and_their_arguments() {
    let bytes = irq_fixture();
    let tree = LinuxFdt::new(&bytes).unwrap();
    let backend = DtBackend::new(tree);
    let root = backend.root().unwrap();
    let uart = node(root, "uart");

    let parent = node(root, "bus")
        .reference("interrupt-parent", 0, 0)
        .unwrap()
        .unwrap();
    assert_eq!(parent.node, node(root, "intc"));
    assert!(parent.args.is_empty());
    assert_eq!(uart.reference("interrupt-parent", 0, 0), Ok(None));

    let first = uart.reference("refs", 0, 1).unwrap().unwrap();
    assert_eq!(first.node, node(root, "intc"));
    assert_eq!(first.args.as_slice(), [3]);

    let second = uart.reference("refs", 1, 1).unwrap().unwrap();
    assert_eq!(second.node, node(root, "intc2"));
    assert_eq!(second.args.as_slice(), [9]);

    assert_eq!(uart.reference("refs", 2, 1), Ok(None));
    assert_eq!(uart.reference("absent", 0, 0), Ok(None));
    assert_eq!(uart.reference("refs", 0, 2), Err(FwError::InvalidValue));
    assert_eq!(
        uart.reference("refs", 0, fwnode::FW_IRQ_SPEC_MAX_PARAMS + 1),
        Err(FwError::BufferTooSmall)
    );
}

#[test]
fn rejects_malformed_interrupt_descriptions() {
    let bytes = irq_fixture();
    let tree = LinuxFdt::new(&bytes).unwrap();
    let backend = DtBackend::new(tree);
    let root = backend.root().unwrap();

    for name in ["bad-parent", "missing-cells", "no-parent", "short"] {
        assert_eq!(
            node(root, name).irq_spec(0),
            Err(FwError::InvalidValue),
            "{name}"
        );
    }
}

#[test]
fn rejects_a_handle_from_another_backend() {
    let first = fixture();
    let second = fixture();
    let first_tree = LinuxFdt::new(&first).unwrap();
    let second_tree = LinuxFdt::new(&second).unwrap();
    let first_backend = DtBackend::new(first_tree);
    let second_backend = DtBackend::new(second_tree);
    let node = first_backend.root().unwrap();
    let other = second_backend.root().unwrap();

    assert_ne!(node, other);
    assert_eq!(node.name(), Some("/"));
}

fn hardware_cpu_fixture(
    address_cells: Option<&[u8]>,
    device_type: Option<&[u8]>,
    reg: Option<&[u8]>,
) -> Vec<u8> {
    let mut blob = Dtb::new();
    blob.begin("");
    blob.begin("cpus");
    if let Some(value) = address_cells {
        blob.property("#address-cells", value);
    }
    blob.property("#size-cells", &cells(&[0]));
    blob.begin("cpu@0");
    if let Some(value) = device_type {
        blob.property("device_type", value);
    }
    if let Some(value) = reg {
        blob.property("reg", value);
    }
    blob.end();
    blob.end();
    blob.end();
    blob.finish()
}

fn cpu_hardware_id(
    address_cells: Option<&[u8]>,
    device_type: Option<&[u8]>,
    reg: Option<&[u8]>,
) -> Result<Option<u64>, FwError> {
    let bytes = hardware_cpu_fixture(address_cells, device_type, reg);
    let tree = LinuxFdt::new(&bytes).unwrap();
    let backend = DtBackend::new(tree);
    let root = backend.root().unwrap();
    node(root, "cpu@0").hardware_cpu_id()
}

#[test]
fn decodes_one_cell_hardware_cpu_identity_including_zero() {
    for identity in [0u32, 5, u32::MAX] {
        assert_eq!(
            cpu_hardware_id(
                Some(&cells(&[1])),
                Some(b"cpu\0"),
                Some(&cells(&[identity])),
            ),
            Ok(Some(u64::from(identity)))
        );
    }
}

#[test]
fn decodes_two_cell_hardware_cpu_identity() {
    assert_eq!(
        cpu_hardware_id(
            Some(&cells(&[2])),
            Some(b"cpu\0"),
            Some(&cells(&[1, 0x2345_6789])),
        ),
        Ok(Some(0x1_2345_6789))
    );
}

#[test]
fn hardware_cpu_identity_absence_is_none() {
    assert_eq!(
        cpu_hardware_id(Some(&cells(&[1])), Some(b"memory\0"), Some(&cells(&[0])),),
        Ok(None)
    );
    assert_eq!(
        cpu_hardware_id(Some(&cells(&[1])), Some(b"cpu\0"), None),
        Ok(None)
    );
    // The X-Kernel CPU-node convention treats a missing #address-cells as one
    // address cell, matching the existing `of` CPU identity helper.
    assert_eq!(
        cpu_hardware_id(None, Some(b"cpu\0"), Some(&cells(&[7]))),
        Ok(Some(7))
    );
}

#[test]
fn rejects_malformed_hardware_cpu_identity() {
    let cases = [
        (cells(&[0]), cells(&[0])),
        (cells(&[3]), cells(&[0, 0, 0])),
        (cells(&[2]), cells(&[0])),
        (cells(&[1]), Vec::from([0, 1])),
        (cells(&[1]), cells(&[5, 0])),
        (Vec::from([0, 1]), cells(&[0])),
    ];
    for (address_cells, reg) in cases {
        assert_eq!(
            cpu_hardware_id(Some(&address_cells), Some(b"cpu\0"), Some(&reg)),
            Err(FwError::InvalidValue),
            "#address-cells={address_cells:?} reg={reg:?}"
        );
    }
}

fn dma_fixture() -> Vec<u8> {
    let mut blob = Dtb::new();
    blob.begin("");
    blob.property("#address-cells", &cells(&[2]));
    blob.property("#size-cells", &cells(&[1]));
    blob.property("dma-coherent", &[]);
    blob.begin("direct");
    blob.end();
    blob.begin("identity");
    blob.property("dma-ranges", &[]);
    blob.end();
    blob.begin("windows");
    blob.property("#address-cells", &cells(&[2]));
    blob.property("#size-cells", &cells(&[1]));
    blob.property(
        "dma-ranges",
        &cells(&[0, 0x1000, 0, 0x8000, 0x100, 0, 0x3000, 0, 0x9000, 0x200]),
    );
    blob.begin("child");
    blob.property("dma-noncoherent", &[]);
    blob.end();
    blob.end();
    blob.begin("single");
    blob.property("#address-cells", &cells(&[2]));
    blob.property("#size-cells", &cells(&[1]));
    blob.property("dma-ranges", &cells(&[0, 0x1000, 0, 0x8000, 0x100]));
    blob.begin("dev");
    blob.end();
    blob.end();
    blob.begin("bad-tuple");
    blob.property("#address-cells", &cells(&[2]));
    blob.property("#size-cells", &cells(&[1]));
    blob.property("dma-ranges", &cells(&[0, 0, 0, 0, 0]));
    blob.end();
    blob.begin("bad-cells");
    blob.property("#address-cells", &cells(&[3]));
    blob.property("dma-ranges", &cells(&[0, 0, 0, 0, 1]));
    blob.end();
    blob.begin("partial");
    blob.property("#address-cells", &cells(&[2]));
    blob.property("#size-cells", &cells(&[1]));
    blob.property(
        "dma-ranges",
        &cells(&[0, 0x1000, 0, 0x8000, 0x100, 0, 0x3000, 0, 0x9000, 0]),
    );
    blob.end();
    blob.begin("conflict");
    blob.property("dma-coherent", &[]);
    blob.property("dma-noncoherent", &[]);
    blob.end();
    blob.begin("duplicate");
    blob.property("dma-ranges", &[]);
    blob.property("dma-ranges", &[]);
    blob.end();
    blob.end();
    blob.finish()
}

#[test]
fn maps_local_dma_declarations_without_ancestor_policy() {
    let bytes = dma_fixture();
    let tree = LinuxFdt::new(&bytes).unwrap();
    let backend = DtBackend::new(tree);
    let root = backend.root().unwrap();

    assert_eq!(root.dma_coherency(), Ok(Some(true)));
    let direct = node(root, "direct");
    assert_eq!(direct.dma_ranges(), Ok(FwDmaRanges::Absent));
    assert_eq!(direct.dma_range(0), Ok(None));
    assert_eq!(direct.dma_coherency(), Ok(None));

    let identity = node(root, "identity");
    assert_eq!(identity.dma_ranges(), Ok(FwDmaRanges::Identity));
    assert_eq!(identity.dma_range(0), Ok(None));

    let windows = node(root, "windows");
    assert_eq!(windows.dma_ranges(), Ok(FwDmaRanges::Windows(2)));
    assert_eq!(
        windows.dma_range(0),
        Ok(Some(FwDmaRange {
            bus_addr: 0x1000,
            parent_addr: 0x8000,
            size: 0x100,
        }))
    );
    assert_eq!(
        windows.dma_range(1),
        Ok(Some(FwDmaRange {
            bus_addr: 0x3000,
            parent_addr: 0x9000,
            size: 0x200,
        }))
    );
    assert_eq!(windows.dma_range(2), Ok(None));
    assert_eq!(
        node(root, "child").dma_coherency(),
        Ok(Some(false)),
        "a parent declaration is not inherited through the neutral accessor"
    );
}

#[test]
fn rejects_malformed_local_dma_declarations_without_partial_views() {
    let bytes = dma_fixture();
    let tree = LinuxFdt::new(&bytes).unwrap();
    let backend = DtBackend::new(tree);
    let root = backend.root().unwrap();

    assert_eq!(
        node(root, "bad-tuple").dma_ranges(),
        Err(FwError::InvalidValue)
    );
    assert_eq!(
        node(root, "bad-tuple").dma_range(0),
        Err(FwError::InvalidValue)
    );
    assert_eq!(
        node(root, "bad-cells").dma_ranges(),
        Err(FwError::Unsupported)
    );
    assert_eq!(
        node(root, "partial").dma_ranges(),
        Err(FwError::InvalidValue)
    );
    assert_eq!(
        node(root, "partial").dma_range(0),
        Err(FwError::InvalidValue),
        "an earlier valid tuple must not hide a malformed later tuple"
    );
    for name in ["conflict"] {
        assert_eq!(
            node(root, name).dma_coherency(),
            Err(FwError::InvalidValue),
            "{name}"
        );
    }
    assert_eq!(
        node(root, "duplicate").dma_ranges(),
        Err(FwError::InvalidValue)
    );
}

#[test]
fn root_dma_ranges_without_a_parent_are_not_guessed() {
    let mut blob = Dtb::new();
    blob.begin("");
    blob.property("#address-cells", &cells(&[2]));
    blob.property("#size-cells", &cells(&[1]));
    blob.property("dma-ranges", &cells(&[0, 0x1000, 0, 0x8000, 0x100]));
    blob.end();
    let bytes = blob.finish();
    let tree = LinuxFdt::new(&bytes).unwrap();
    let backend = DtBackend::new(tree);
    let root = backend.root().unwrap();

    assert_eq!(root.dma_ranges(), Err(FwError::InvalidValue));
    assert_eq!(root.dma_range(0), Err(FwError::InvalidValue));
}

#[test]
fn resolved_dma_facts_cross_the_neutral_contract() {
    let bytes = dma_fixture();
    let tree = LinuxFdt::new(&bytes).unwrap();
    let backend = DtBackend::new(tree);
    let root = backend.root().unwrap();

    assert_eq!(
        node(root, "dev").dma_window(),
        Ok(Some(FwDmaWindow {
            bus_addr: 0x1000,
            cpu_addr: 0x8000,
            size: 0x100,
        }))
    );
    assert_eq!(node(root, "dev").dma_effective_coherency(), Ok(Some(true)));
    assert_eq!(
        node(root, "child").dma_window(),
        Err(FwDmaError::MultiWindow)
    );
    assert_eq!(
        node(root, "direct").dma_window(),
        Ok(None),
        "an absent declaration crosses the neutral boundary as absence, not an error"
    );
    assert_eq!(
        node(root, "child").dma_effective_coherency(),
        Ok(Some(false))
    );
    assert_eq!(
        node(root, "dev").dma_effective_coherency(),
        Ok(Some(true)),
        "the nearest declaration wins over the platform default"
    );
}
