// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use rs_fdtree::LinuxFdt;

static DTB: &[u8] = include_bytes!("../dtb/test.dtb");

#[test]
fn offsets_round_trip_across_copied_and_reconstructed_views() {
    let tree = LinuxFdt::new(DTB).unwrap();
    let copy = tree;
    let rebuilt = LinuxFdt::new(tree.as_bytes()).unwrap();
    for node in tree.all_nodes() {
        assert!(core::ptr::eq(node.as_tree(), &tree));
        for wrapper in [&copy, &rebuilt] {
            let found = wrapper.node_at_offset(node.offset()).unwrap();
            assert_eq!(found.offset(), node.offset());
            assert_eq!(found.name, node.name);
        }
    }
    let soc = tree.find_node("/soc").unwrap();
    assert_eq!(tree.node_at_offset(soc.offset()).unwrap().name, soc.name);
    assert_eq!(
        tree.find_node("/").unwrap().offset(),
        tree.all_nodes().next().unwrap().offset()
    );
}

#[test]
fn only_actual_node_boundaries_resolve() {
    let tree = LinuxFdt::new(DTB).unwrap();
    let offsets: Vec<_> = tree.all_nodes().map(|node| node.offset()).collect();
    for offset in 0..DTB.len() as u32 {
        assert_eq!(
            tree.node_at_offset(offset).is_some(),
            offsets.contains(&offset)
        );
    }
    assert!(tree.node_at_offset(u32::MAX).is_none());
}
