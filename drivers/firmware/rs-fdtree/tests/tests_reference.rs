// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

mod support;

use rs_fdtree::{FdtNode, LinuxFdt, PropertyError, ReferenceError};
use support::Dtb;

fn cells(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_be_bytes())
        .collect()
}

fn node<'b, 'a>(tree: &'b LinuxFdt<'a>, name: &str) -> FdtNode<'b, 'a> {
    tree.all_nodes().find(|node| node.name == name).unwrap()
}

fn fixture() -> Vec<u8> {
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
    blob.property("linux,phandle", &cells(&[2]));
    blob.end();
    blob.begin("intc3");
    blob.property("compatible", b"test,bad-controller\0");
    blob.property("interrupt-controller", &[]);
    blob.property("phandle", &cells(&[3]));
    blob.end();
    blob.begin("bus");
    blob.property("interrupt-parent", &cells(&[1]));
    blob.begin("mid");
    blob.begin("uart");
    blob.property("compatible", b"arm,pl011\0");
    blob.property("interrupts", &cells(&[0, 5, 4]));
    blob.property("refs", &cells(&[1, 3, 2, 9]));
    blob.end();
    blob.end();
    blob.end();
    blob.begin("bus2");
    blob.property("interrupt-parent", &cells(&[2]));
    blob.begin("mid2");
    blob.begin("uart2");
    blob.property("interrupts", &cells(&[7]));
    blob.end();
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
    blob.begin("duplicate");
    blob.property("interrupt-parent", &cells(&[1]));
    blob.property("interrupt-parent", &cells(&[2]));
    blob.property("interrupts", &cells(&[5, 0, 4]));
    blob.begin("dupchild");
    blob.property("interrupts", &cells(&[5, 0, 4]));
    blob.end();
    blob.end();
    blob.begin("badtree");
    blob.property("interrupt-parent", &[0, 0]);
    blob.begin("badchild");
    blob.property("interrupts", &cells(&[5]));
    blob.end();
    blob.begin("goodchild");
    blob.property("interrupt-parent", &cells(&[1]));
    blob.property("interrupts", &cells(&[0, 5, 4]));
    blob.end();
    blob.end();
    blob.end();
    blob.finish()
}

#[test]
fn resolves_phandles_and_the_legacy_alias() {
    let bytes = fixture();
    let tree = LinuxFdt::new(&bytes).unwrap();

    assert_eq!(tree.node_by_phandle(1).unwrap().name, "intc");
    assert_eq!(tree.node_by_phandle(2).unwrap().name, "intc2");
    assert!(tree.node_by_phandle(0).is_none());
    assert!(tree.node_by_phandle(u32::MAX).is_none());
    assert!(tree.node_by_phandle(99).is_none());

    assert_eq!(node(&tree, "intc").phandle(), Ok(1));
    assert_eq!(node(&tree, "intc2").phandle(), Ok(2));
    assert_eq!(node(&tree, "bare").phandle(), Err(PropertyError::Missing));
}

#[test]
fn inherits_interrupt_parent_through_ancestors() {
    let bytes = fixture();
    let tree = LinuxFdt::new(&bytes).unwrap();
    let uart = node(&tree, "uart");

    let parent = uart.interrupt_parent().unwrap().unwrap();
    assert_eq!(parent.offset(), node(&tree, "intc").offset());

    let spec = uart.irq_spec(0).unwrap().unwrap();
    assert_eq!(spec.controller.offset(), node(&tree, "intc").offset());
    assert_eq!(spec.params.iter().collect::<Vec<_>>(), [0, 5, 4]);
    assert!(uart.irq_spec(1).unwrap().is_none());
}

#[test]
fn nearer_interrupt_parent_overrides_inheritance() {
    let bytes = fixture();
    let tree = LinuxFdt::new(&bytes).unwrap();
    let spec = node(&tree, "uart2").irq_spec(0).unwrap().unwrap();

    assert_eq!(spec.controller.offset(), node(&tree, "intc2").offset());
    assert_eq!(spec.params.iter().collect::<Vec<_>>(), [7]);
}

#[test]
fn path_and_relationship_walks_keep_the_inheritance_cache() {
    let bytes = fixture();
    let tree = LinuxFdt::new(&bytes).unwrap();

    let uart = tree.find_node("/bus/mid/uart").unwrap();
    let spec = uart.irq_spec(0).unwrap().unwrap();
    assert_eq!(spec.controller.offset(), node(&tree, "intc").offset());

    let mid = uart.parent().unwrap();
    let parent = mid.interrupt_parent().unwrap().unwrap();
    assert_eq!(parent.offset(), node(&tree, "intc").offset());
}

#[test]
fn malformed_inherited_declaration_is_not_treated_as_absence() {
    let bytes = fixture();
    let tree = LinuxFdt::new(&bytes).unwrap();

    assert!(matches!(
        node(&tree, "badchild").interrupt_parent(),
        Err(ReferenceError::Property(PropertyError::InvalidEncoding))
    ));
    assert!(matches!(
        node(&tree, "badchild").irq_spec(0),
        Err(ReferenceError::Property(PropertyError::InvalidEncoding))
    ));
    let spec = node(&tree, "goodchild").irq_spec(0).unwrap().unwrap();
    assert_eq!(spec.controller.offset(), node(&tree, "intc").offset());
    assert!(matches!(
        node(&tree, "dupchild").irq_spec(0),
        Err(ReferenceError::Property(PropertyError::Duplicate))
    ));
}

#[test]
fn enumerates_interrupts_extended_entries() {
    let bytes = fixture();
    let tree = LinuxFdt::new(&bytes).unwrap();
    let device = node(&tree, "device");

    let first = device.irq_spec(0).unwrap().unwrap();
    assert_eq!(first.controller.offset(), node(&tree, "intc").offset());
    assert_eq!(first.params.iter().collect::<Vec<_>>(), [0, 6, 4]);

    let second = device.irq_spec(1).unwrap().unwrap();
    assert_eq!(second.controller.offset(), node(&tree, "intc2").offset());
    assert_eq!(second.params.iter().collect::<Vec<_>>(), [7]);

    assert!(device.irq_spec(2).unwrap().is_none());
    assert!(node(&tree, "bare").irq_spec(0).unwrap().is_none());
}

#[test]
fn resolves_references_and_their_arguments() {
    let bytes = fixture();
    let tree = LinuxFdt::new(&bytes).unwrap();
    let uart = node(&tree, "uart");
    let bus = node(&tree, "bus");

    let parent = bus.reference("interrupt-parent", 0, 0).unwrap().unwrap();
    assert_eq!(parent.node.offset(), node(&tree, "intc").offset());
    assert!(parent.args.is_empty());
    assert!(uart.reference("interrupt-parent", 0, 0).unwrap().is_none());

    let first = uart.reference("refs", 0, 1).unwrap().unwrap();
    assert_eq!(first.node.offset(), node(&tree, "intc").offset());
    assert_eq!(first.args.iter().collect::<Vec<_>>(), [3]);

    let second = uart.reference("refs", 1, 1).unwrap().unwrap();
    assert_eq!(second.node.offset(), node(&tree, "intc2").offset());
    assert_eq!(second.args.iter().collect::<Vec<_>>(), [9]);

    assert!(uart.reference("refs", 2, 1).unwrap().is_none());
    assert!(uart.reference("absent", 0, 0).unwrap().is_none());
    assert!(matches!(
        uart.reference("refs", 0, 2),
        Err(ReferenceError::Property(PropertyError::InvalidEncoding))
    ));
}

#[test]
fn cell_reads_reject_a_short_output_buffer() {
    let bytes = fixture();
    let tree = LinuxFdt::new(&bytes).unwrap();
    let refs = node(&tree, "uart")
        .property("refs")
        .unwrap()
        .cells()
        .unwrap();
    let mut output = [0u32; 2];

    assert_eq!(
        refs.read_into(&mut output),
        Err(PropertyError::BufferTooSmall { required: 4 })
    );
    assert_eq!(output, [0, 0]);
}

#[test]
fn rejects_malformed_interrupt_descriptions() {
    let bytes = fixture();
    let tree = LinuxFdt::new(&bytes).unwrap();

    assert!(matches!(
        node(&tree, "bad-parent").irq_spec(0),
        Err(ReferenceError::UnresolvedPhandle(9))
    ));
    assert!(matches!(
        node(&tree, "missing-cells").irq_spec(0),
        Err(ReferenceError::Property(PropertyError::Missing))
    ));
    assert!(matches!(
        node(&tree, "no-parent").irq_spec(0),
        Err(ReferenceError::MissingInterruptParent)
    ));
    assert!(matches!(
        node(&tree, "short").irq_spec(0),
        Err(ReferenceError::Property(PropertyError::InvalidEncoding))
    ));
    assert!(matches!(
        node(&tree, "duplicate").irq_spec(0),
        Err(ReferenceError::Property(PropertyError::Duplicate))
    ));
}
