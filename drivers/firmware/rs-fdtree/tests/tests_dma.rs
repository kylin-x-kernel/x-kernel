// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

mod support;

use rs_fdtree::{
    DeviceDmaError, DeviceDmaWindow, DmaRange, DmaRangeError, DmaRanges, LinuxFdt, PropertyError,
    parse_dma_ranges,
};
use support::Dtb;

fn encode(cells: &[u32]) -> Vec<u8> {
    cells.iter().flat_map(|cell| cell.to_be_bytes()).collect()
}

fn decode(bytes: &[u8], widths: [u32; 3]) -> Result<Vec<DmaRange>, DmaRangeError> {
    match parse_dma_ranges(Some(bytes), widths[0], widths[1], widths[2])? {
        DmaRanges::Windows(windows) => windows.collect(),
        _ => Ok(Vec::new()),
    }
}

#[test]
fn dma_missing_and_empty_are_distinct() {
    assert!(matches!(
        parse_dma_ranges(None, 0, 0, 0),
        Ok(DmaRanges::Absent)
    ));
    assert!(matches!(
        parse_dma_ranges(Some(&[]), 0, 0, 0),
        Ok(DmaRanges::Identity)
    ));
}

#[test]
fn dma_preserves_multiple_windows_and_lower_bounds() {
    let bytes = encode(&[0x1000, 0x8000, 0x100, 0x3000, 0x9000, 0x200]);
    assert_eq!(
        decode(&bytes, [1, 1, 1]),
        Ok(vec![
            DmaRange {
                bus_addr: 0x1000,
                parent_addr: 0x8000,
                size: 0x100
            },
            DmaRange {
                bus_addr: 0x3000,
                parent_addr: 0x9000,
                size: 0x200
            },
        ])
    );
}

#[test]
fn dma_keeps_full_width_addresses_without_signed_offset_conversion() {
    let bytes = encode(&[u32::MAX, u32::MAX - 15, 0, 0, 0, 16]);
    assert_eq!(
        decode(&bytes, [2, 2, 2]),
        Ok(vec![DmaRange {
            bus_addr: u64::MAX - 15,
            parent_addr: 0,
            size: 16,
        }])
    );
    let bytes = encode(&[0x1000, 1, 0, 0, 0x100]);
    assert_eq!(
        decode(&bytes, [1, 2, 2]),
        Ok(vec![DmaRange {
            bus_addr: 0x1000,
            parent_addr: 1 << 32,
            size: 0x100,
        }])
    );
}

#[test]
fn dma_rejects_bad_layouts() {
    for widths in [[0, 1, 1], [1, 3, 1], [1, 1, u32::MAX]] {
        assert_eq!(
            decode(&[0; 12], widths),
            Err(DmaRangeError::UnsupportedCells)
        );
    }
    for length in [1, 4, 11, 13, 23] {
        assert_eq!(
            decode(&vec![0; length], [1, 1, 1]),
            Err(DmaRangeError::TruncatedTuple)
        );
    }
}

#[test]
fn dma_rejects_zero_size_and_both_address_overflows() {
    assert_eq!(
        decode(&encode(&[0, 0, 0]), [1, 1, 1]),
        Err(DmaRangeError::EmptyWindow)
    );
    for cells in [[u32::MAX, u32::MAX, 0, 0, 2], [0, 0, u32::MAX, u32::MAX, 2]] {
        assert_eq!(
            decode(&encode(&cells), [2, 2, 1]),
            Err(DmaRangeError::AddressOverflow)
        );
    }
    let bytes = encode(&[0x1000, 0x8000, 0x100, 0x3000, 0x9000, 0]);
    assert_eq!(decode(&bytes, [1, 1, 1]), Err(DmaRangeError::EmptyWindow));
}

#[test]
fn node_dma_ranges_preserve_multiple_windows_and_use_local_cell_defaults() {
    let mut blob = Dtb::new();
    blob.begin("");
    blob.begin("bus");
    blob.property(
        "dma-ranges",
        &encode(&[0, 0x100, 0, 0x8000, 0x100, 0, 0x400, 0, 0x9000, 0x80]),
    );
    blob.end();
    blob.end();
    let bytes = blob.finish();
    let tree = LinuxFdt::new(&bytes).unwrap();
    let node = tree.find_node("/bus").unwrap();
    let DmaRanges::Windows(windows) = node.dma_ranges().unwrap() else {
        panic!("expected declared windows");
    };
    let windows = windows.collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(windows.len(), 2);
    assert_eq!(windows[0].parent_addr, 0x8000);
    assert_eq!(windows[1].bus_addr, 0x400);
}

#[test]
fn local_coherency_does_not_inherit_or_invent_a_host_default() {
    let mut blob = Dtb::new();
    blob.begin("");
    blob.property("dma-coherent", &[]);
    blob.begin("device");
    blob.end();
    blob.begin("noncoherent");
    blob.property("dma-noncoherent", &[]);
    blob.end();
    blob.end();
    let bytes = blob.finish();
    let tree = LinuxFdt::new(&bytes).unwrap();
    let node = tree.find_node("/device").unwrap();
    assert_eq!(node.dma_coherency(), Ok(None));
    assert_eq!(node.parent().unwrap().dma_coherency(), Ok(Some(true)));
    assert_eq!(
        tree.find_node("/noncoherent").unwrap().dma_coherency(),
        Ok(Some(false))
    );
    assert!(matches!(node.dma_ranges(), Ok(DmaRanges::Absent)));
}

#[test]
fn root_dma_declarations_distinguish_identity_from_missing_parent() {
    for property in [None, Some(Vec::new()), Some(encode(&[0, 0, 1]))] {
        let mut blob = Dtb::new();
        blob.begin("");
        if let Some(value) = &property {
            blob.property("dma-ranges", value);
        }
        blob.end();
        let bytes = blob.finish();
        let tree = LinuxFdt::new(&bytes).unwrap();
        let root = tree.all_nodes().next().unwrap();
        match property {
            None => assert!(matches!(root.dma_ranges(), Ok(DmaRanges::Absent))),
            Some(value) if value.is_empty() => {
                assert!(matches!(root.dma_ranges(), Ok(DmaRanges::Identity)));
            }
            Some(_) => {
                assert!(matches!(
                    root.dma_ranges(),
                    Err(DmaRangeError::MissingParent)
                ));
            }
        }
    }
}

#[test]
fn local_dma_readers_reject_duplicate_and_malformed_properties() {
    let mut blob = Dtb::new();
    blob.begin("");
    blob.begin("duplicate");
    blob.property("dma-ranges", &[]);
    blob.property("dma-ranges", &[]);
    blob.end();
    blob.begin("bad-cells");
    blob.property("#address-cells", &[1]);
    blob.property("dma-ranges", &encode(&[0, 0, 0, 0, 1]));
    blob.end();
    blob.begin("conflict");
    blob.property("dma-coherent", &[]);
    blob.property("dma-noncoherent", &[]);
    blob.end();
    blob.begin("nonempty");
    blob.property("dma-coherent", &[1]);
    blob.end();
    blob.begin("duplicate-coherent");
    blob.property("dma-coherent", &[]);
    blob.property("dma-coherent", &[]);
    blob.end();
    blob.end();
    let bytes = blob.finish();
    let tree = LinuxFdt::new(&bytes).unwrap();
    assert!(matches!(
        tree.find_node("/duplicate").unwrap().dma_ranges(),
        Err(DmaRangeError::Property(PropertyError::Duplicate))
    ));
    assert!(matches!(
        tree.find_node("/bad-cells").unwrap().dma_ranges(),
        Err(DmaRangeError::Property(PropertyError::InvalidEncoding))
    ));
    for path in ["/conflict", "/nonempty"] {
        assert_eq!(
            tree.find_node(path).unwrap().dma_coherency(),
            Err(PropertyError::InvalidEncoding)
        );
    }
    assert_eq!(
        tree.find_node("/duplicate-coherent")
            .unwrap()
            .dma_coherency(),
        Err(PropertyError::Duplicate)
    );
}

// -- Resolved device DMA windows (DT ancestry rules) --------------------------

fn cells(blob: &mut Dtb, name: &str, values: &[u32]) {
    blob.property(name, &encode(values));
}

/// `root -> mid -> bus -> dev` with one window at each bus.
fn ancestor_fixture(mid_size: u32, mid_parent: u32, bus_size: u32) -> Vec<u8> {
    let mut blob = Dtb::new();
    blob.begin("");
    cells(&mut blob, "#address-cells", &[1]);
    cells(&mut blob, "#size-cells", &[1]);
    blob.begin("mid");
    cells(&mut blob, "#address-cells", &[1]);
    cells(&mut blob, "#size-cells", &[1]);
    cells(&mut blob, "dma-ranges", &[0x2000, mid_parent, mid_size]);
    blob.begin("bus");
    cells(&mut blob, "#address-cells", &[1]);
    cells(&mut blob, "#size-cells", &[1]);
    cells(&mut blob, "dma-ranges", &[0x1000, 0x2000, bus_size]);
    blob.begin("dev");
    blob.end();
    blob.end();
    blob.end();
    blob.end();
    blob.finish()
}

#[test]
fn device_window_translates_across_every_ancestor_boundary() {
    let bytes = ancestor_fixture(0x4000, 0x8000, 0x1000);
    let tree = LinuxFdt::new(&bytes).unwrap();
    let dev = tree.find_node("/mid/bus/dev").unwrap();
    assert_eq!(
        dev.device_dma_window(),
        Ok(Some(DeviceDmaWindow {
            bus_addr: 0x1000,
            cpu_addr: 0x8000,
            size: 0x1000,
        }))
    );
}

#[test]
fn ancestor_window_must_cover_the_whole_device_window() {
    let bytes = ancestor_fixture(0x800, 0x8000, 0x1000);
    let tree = LinuxFdt::new(&bytes).unwrap();
    let dev = tree.find_node("/mid/bus/dev").unwrap();
    assert_eq!(dev.device_dma_window(), Err(DeviceDmaError::InvalidWindow));
}

#[test]
fn ambiguous_ancestor_windows_are_not_guessed() {
    let mut blob = Dtb::new();
    blob.begin("");
    cells(&mut blob, "#address-cells", &[1]);
    cells(&mut blob, "#size-cells", &[1]);
    blob.begin("mid");
    cells(&mut blob, "#address-cells", &[1]);
    cells(&mut blob, "#size-cells", &[1]);
    cells(
        &mut blob,
        "dma-ranges",
        &[0x2000, 0x8000, 0x2000, 0x2000, 0x9000, 0x2000],
    );
    blob.begin("bus");
    cells(&mut blob, "#address-cells", &[1]);
    cells(&mut blob, "#size-cells", &[1]);
    cells(&mut blob, "dma-ranges", &[0x1000, 0x2000, 0x1000]);
    blob.begin("dev");
    blob.end();
    blob.end();
    blob.end();
    blob.end();
    let bytes = blob.finish();
    let tree = LinuxFdt::new(&bytes).unwrap();
    let dev = tree.find_node("/mid/bus/dev").unwrap();
    assert_eq!(
        dev.device_dma_window(),
        Err(DeviceDmaError::UnsupportedTranslation)
    );
}

#[test]
fn empty_declaration_is_skipped_but_a_later_absence_is_an_error() {
    let mut blob = Dtb::new();
    blob.begin("");
    cells(&mut blob, "#address-cells", &[1]);
    cells(&mut blob, "#size-cells", &[1]);
    blob.begin("mid");
    cells(&mut blob, "#address-cells", &[1]);
    cells(&mut blob, "#size-cells", &[1]);
    cells(&mut blob, "dma-ranges", &[0x2000, 0x8000, 0x1000]);
    blob.begin("bus");
    cells(&mut blob, "#address-cells", &[1]);
    cells(&mut blob, "#size-cells", &[1]);
    blob.property("dma-ranges", &[]);
    blob.begin("dev");
    blob.end();
    blob.end();
    blob.end();
    blob.end();
    let bytes = blob.finish();
    let tree = LinuxFdt::new(&bytes).unwrap();
    assert_eq!(
        tree.find_node("/mid/bus/dev").unwrap().device_dma_window(),
        Ok(Some(DeviceDmaWindow {
            bus_addr: 0x2000,
            cpu_addr: 0x8000,
            size: 0x1000,
        }))
    );

    let mut blob = Dtb::new();
    blob.begin("");
    cells(&mut blob, "#address-cells", &[1]);
    cells(&mut blob, "#size-cells", &[1]);
    blob.begin("mid");
    blob.begin("bus");
    blob.property("dma-ranges", &[]);
    blob.begin("dev");
    blob.end();
    blob.end();
    blob.end();
    blob.end();
    let bytes = blob.finish();
    let tree = LinuxFdt::new(&bytes).unwrap();
    assert_eq!(
        tree.find_node("/mid/bus/dev").unwrap().device_dma_window(),
        Err(DeviceDmaError::Missing)
    );
}

#[test]
fn missing_window_is_never_identity() {
    let mut blob = Dtb::new();
    blob.begin("");
    blob.begin("dev");
    blob.end();
    blob.end();
    let bytes = blob.finish();
    let tree = LinuxFdt::new(&bytes).unwrap();
    assert_eq!(
        tree.find_node("/dev").unwrap().device_dma_window(),
        Err(DeviceDmaError::Missing)
    );
}

#[test]
fn only_one_window_is_representable() {
    let mut blob = Dtb::new();
    blob.begin("");
    cells(&mut blob, "#address-cells", &[1]);
    cells(&mut blob, "#size-cells", &[1]);
    blob.begin("bus");
    cells(&mut blob, "#address-cells", &[1]);
    cells(&mut blob, "#size-cells", &[1]);
    cells(
        &mut blob,
        "dma-ranges",
        &[0x1000, 0x8000, 0x100, 0x3000, 0x9000, 0x100],
    );
    blob.begin("dev");
    blob.end();
    blob.end();
    blob.end();
    let bytes = blob.finish();
    let tree = LinuxFdt::new(&bytes).unwrap();
    assert_eq!(
        tree.find_node("/bus/dev").unwrap().device_dma_window(),
        Err(DeviceDmaError::MultiWindow)
    );
}

#[test]
fn interconnect_overrides_and_iommu_routing_are_rejected() {
    for (on_device, property, expected) in [
        (
            true,
            "interconnect-names",
            DeviceDmaError::UnsupportedParent,
        ),
        (false, "interconnects", DeviceDmaError::UnsupportedParent),
        (true, "iommus", DeviceDmaError::Iommu),
        (false, "iommu-map", DeviceDmaError::Iommu),
        (false, "iommu-map-mask", DeviceDmaError::Iommu),
    ] {
        let mut blob = Dtb::new();
        blob.begin("");
        cells(&mut blob, "#address-cells", &[1]);
        cells(&mut blob, "#size-cells", &[1]);
        blob.begin("bus");
        cells(&mut blob, "#address-cells", &[1]);
        cells(&mut blob, "#size-cells", &[1]);
        cells(&mut blob, "dma-ranges", &[0x1000, 0x8000, 0x100]);
        if !on_device {
            blob.property(property, &[]);
        }
        blob.begin("dev");
        if on_device {
            blob.property(property, &[]);
        }
        blob.end();
        blob.end();
        blob.end();
        let bytes = blob.finish();
        let tree = LinuxFdt::new(&bytes).unwrap();
        assert_eq!(
            tree.find_node("/bus/dev").unwrap().device_dma_window(),
            Err(expected),
            "{property} on {}",
            if on_device { "device" } else { "bus" }
        );
    }
}

#[test]
fn malformed_windows_report_the_declaration_error() {
    let cases: [(&[u32], [u32; 3], DmaRangeError); 4] = [
        (&[0x1000, 0x8000, 0], [1, 1, 1], DmaRangeError::EmptyWindow),
        (
            &[0, 0x1000, u32::MAX, u32::MAX, 0, 2],
            [2, 2, 2],
            DmaRangeError::AddressOverflow,
        ),
        (&[0, 0, 0, 0, 0], [1, 1, 1], DmaRangeError::TruncatedTuple),
        (&[0, 0, 0, 0, 0], [3, 1, 1], DmaRangeError::UnsupportedCells),
    ];
    for (ranges, widths, expected) in cases {
        let mut blob = Dtb::new();
        blob.begin("");
        cells(&mut blob, "#address-cells", &[widths[1]]);
        cells(&mut blob, "#size-cells", &[widths[2]]);
        blob.begin("bus");
        cells(&mut blob, "#address-cells", &[widths[0]]);
        cells(&mut blob, "#size-cells", &[widths[2]]);
        cells(&mut blob, "dma-ranges", ranges);
        blob.begin("dev");
        blob.end();
        blob.end();
        blob.end();
        let bytes = blob.finish();
        let tree = LinuxFdt::new(&bytes).unwrap();
        assert_eq!(
            tree.find_node("/bus/dev").unwrap().device_dma_window(),
            Err(DeviceDmaError::Declaration(expected)),
            "widths {widths:?}"
        );
    }
}

#[test]
fn declaration_on_the_root_has_no_parent_boundary() {
    let mut blob = Dtb::new();
    blob.begin("");
    cells(&mut blob, "#address-cells", &[1]);
    cells(&mut blob, "#size-cells", &[1]);
    cells(&mut blob, "dma-ranges", &[0x1000, 0x8000, 0x100]);
    blob.begin("dev");
    blob.end();
    blob.end();
    let bytes = blob.finish();
    let tree = LinuxFdt::new(&bytes).unwrap();
    assert_eq!(
        tree.find_node("/dev").unwrap().device_dma_window(),
        Err(DeviceDmaError::Declaration(DmaRangeError::MissingParent))
    );
}

#[test]
fn coherency_inheritance_uses_the_nearest_declaration() {
    let mut blob = Dtb::new();
    blob.begin("");
    blob.property("dma-coherent", &[]);
    blob.begin("mid");
    blob.property("dma-noncoherent", &[]);
    blob.begin("bus");
    blob.begin("dev");
    blob.end();
    blob.begin("own");
    blob.property("dma-coherent", &[]);
    blob.end();
    blob.end();
    blob.end();
    blob.end();
    let bytes = blob.finish();
    let tree = LinuxFdt::new(&bytes).unwrap();
    assert_eq!(
        tree.find_node("/").unwrap().inherited_dma_coherency(),
        Ok(Some(true))
    );
    assert_eq!(
        tree.find_node("/mid").unwrap().inherited_dma_coherency(),
        Ok(Some(false))
    );
    assert_eq!(
        tree.find_node("/mid/bus/dev")
            .unwrap()
            .inherited_dma_coherency(),
        Ok(Some(false))
    );
    assert_eq!(
        tree.find_node("/mid/bus/own")
            .unwrap()
            .inherited_dma_coherency(),
        Ok(Some(true))
    );
}

#[test]
fn coherency_absence_stays_unset_for_the_platform_to_decide() {
    let mut blob = Dtb::new();
    blob.begin("");
    blob.begin("dev");
    blob.end();
    blob.end();
    let bytes = blob.finish();
    let tree = LinuxFdt::new(&bytes).unwrap();
    assert_eq!(
        tree.find_node("/dev").unwrap().inherited_dma_coherency(),
        Ok(None)
    );
}
