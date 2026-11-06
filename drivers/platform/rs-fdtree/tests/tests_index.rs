// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

mod support;

use rs_fdtree::{FdtNode, LinuxFdt, NodeIndex};
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
    blob.begin("both");
    blob.property("phandle", &cells(&[8]));
    blob.property("linux,phandle", &cells(&[9]));
    blob.end();
    blob.begin("short-phandle");
    blob.property("phandle", &[0, 0]);
    blob.end();
    blob.begin("zero");
    blob.property("phandle", &cells(&[0]));
    blob.end();
    blob.begin("max");
    blob.property("phandle", &cells(&[u32::MAX]));
    blob.end();
    blob.begin("dup-first");
    blob.property("phandle", &cells(&[7]));
    blob.end();
    blob.begin("dup-second");
    blob.property("phandle", &cells(&[7]));
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
    blob.end();
    blob.finish()
}

#[test]
fn indexes_every_node_by_offset_and_phandle() {
    let bytes = fixture();
    let tree = LinuxFdt::new(&bytes).unwrap();
    let index = tree.node_index();

    let names: Vec<_> = tree.all_nodes().map(|node| node.name).collect();
    assert_eq!(index.len(), names.len());
    assert!(!index.is_empty());
    assert_eq!(
        index.nodes().map(|node| node.name).collect::<Vec<_>>(),
        names
    );
    for node in tree.all_nodes() {
        let indexed = index.node_by_offset(node.offset()).unwrap();
        assert_eq!(indexed.name, node.name);
        assert_eq!(indexed.offset(), node.offset());
    }

    assert_eq!(index.node_by_phandle(1).unwrap().name, "intc");
    assert_eq!(index.node_by_phandle(2).unwrap().name, "intc2");
    assert_eq!(index.node_by_phandle(8).unwrap().name, "both");
    assert!(index.node_by_phandle(9).is_none());
    assert!(index.node_by_phandle(0).is_none());
    assert!(index.node_by_phandle(u32::MAX).is_none());
    assert!(index.node_by_phandle(99).is_none());
    assert_eq!(index.node_by_offset(0).unwrap().name, "/");
    assert!(index.node_by_offset(u32::MAX).is_none());
}

#[test]
fn keeps_the_first_duplicate_and_skips_malformed_phandles() {
    let bytes = fixture();
    let tree = LinuxFdt::new(&bytes).unwrap();
    let index = tree.node_index();

    assert_eq!(index.node_by_phandle(7).unwrap().name, "dup-first");
    assert!(index.node_by_phandle(5).is_none());
    assert_eq!(
        index
            .nodes()
            .filter(|node| node.name.starts_with("dup-"))
            .count(),
        2
    );
}

#[test]
fn indexed_resolution_matches_the_linear_path() {
    let bytes = fixture();
    let tree = LinuxFdt::new(&bytes).unwrap();
    let index = tree.node_index();
    let uart = node(&tree, "uart");
    let uart_indexed = index.node_by_offset(uart.offset()).unwrap();

    assert_eq!(
        uart_indexed
            .interrupt_parent_with(&index)
            .unwrap()
            .unwrap()
            .offset(),
        node(&tree, "intc").offset()
    );

    let linear = uart.irq_spec(0).unwrap().unwrap();
    let indexed = uart_indexed.irq_spec_with(&index, 0).unwrap().unwrap();
    assert_eq!(indexed.controller.offset(), linear.controller.offset());
    assert_eq!(
        indexed.params.iter().collect::<Vec<_>>(),
        linear.params.iter().collect::<Vec<_>>()
    );
    assert!(uart_indexed.irq_spec_with(&index, 1).unwrap().is_none());

    let device = index
        .node_by_offset(node(&tree, "device").offset())
        .unwrap();
    let second = device.irq_spec_with(&index, 1).unwrap().unwrap();
    assert_eq!(second.controller.offset(), node(&tree, "intc2").offset());
    assert_eq!(second.params.iter().collect::<Vec<_>>(), [7]);

    let first = uart_indexed
        .reference_with(&index, "refs", 0, 1)
        .unwrap()
        .unwrap();
    assert_eq!(first.node.offset(), node(&tree, "intc").offset());
    assert_eq!(first.args.iter().collect::<Vec<_>>(), [3]);

    let parent = index
        .node_by_offset(node(&tree, "bus").offset())
        .unwrap()
        .reference_with(&index, "interrupt-parent", 0, 0)
        .unwrap()
        .unwrap();
    assert_eq!(parent.node.offset(), node(&tree, "intc").offset());
}

#[test]
fn the_index_owns_a_copy_of_the_tree_handle() {
    let bytes = fixture();
    let tree = LinuxFdt::new(&bytes).unwrap();
    let index = NodeIndex::new(tree);

    assert_eq!(index.tree().total_size(), tree.total_size());
    assert_eq!(index.node_by_phandle(1).unwrap().name, "intc");
}
