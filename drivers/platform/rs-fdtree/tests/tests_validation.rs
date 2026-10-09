// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use rs_fdtree::{FdtError, LinuxFdt};

fn words(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_be_bytes())
        .collect()
}

fn blob(structure: &[u8], strings: &[u8]) -> Vec<u8> {
    let strings_offset = 56 + structure.len();
    let mut bytes = words(&[
        0xd00dfeed,
        (strings_offset + strings.len()) as u32,
        56,
        strings_offset as u32,
        40,
        17,
        16,
        0,
        strings.len() as u32,
        structure.len() as u32,
    ]);
    bytes.extend_from_slice(&[0; 16]);
    bytes.extend_from_slice(structure);
    bytes.extend_from_slice(strings);
    bytes
}

fn valid_blob() -> Vec<u8> {
    blob(
        &words(&[
            1, 0, 3, 1, 0, 0xff000000, 1, 0x6368696c, 0x64000000, 2, 2, 9,
        ]),
        b"prop\0",
    )
}

fn set_header(bytes: &mut [u8], field: usize, value: u32) {
    bytes[field * 4..field * 4 + 4].copy_from_slice(&value.to_be_bytes());
}

#[test]
fn malformed_tokens_and_names_fail_construction() {
    for tokens in [
        &[][..],
        &[1],
        &[1, 0],
        &[1, 0, 2],
        &[1, 0, 5],
        &[1, 0, 9],
        &[1, 0, 2, 2, 9],
        &[1, 0, 2, 1, 0, 2, 9],
        &[1, 0, 2, 9, 4],
        &[1, 0x78000000, 2, 9],
        &[1, 0, 1, 0, 2, 2, 9],
        &[1, 0, 1, 0xff000000, 2, 2, 9],
        &[1, 0, 1, 0x612f6200, 2, 2, 9],
        &[1, 0, 1, 0x78787878],
    ] {
        let bytes = blob(&words(tokens), b"");
        assert_eq!(
            LinuxFdt::new(&bytes).err(),
            Some(FdtError::ParseError),
            "{tokens:x?}"
        );
    }
}

#[test]
fn malformed_properties_fail_before_any_lookup() {
    for tokens in [
        &[1, 0, 3][..],
        &[1, 0, 3, 0],
        &[1, 0, 3, 20, 0, 2, 9],
        &[1, 0, 3, u32::MAX, 0, 2, 9],
        &[1, 0, 3, 0, 99, 2, 9],
        &[1, 0, 1, 0x78000000, 2, 3, 0, 0, 2, 9],
        &[3, 0, 0, 1, 0, 2, 9],
    ] {
        let bytes = blob(&words(tokens), b"prop\0");
        assert_eq!(
            LinuxFdt::new(&bytes).err(),
            Some(FdtError::ParseError),
            "{tokens:x?}"
        );
    }
    for strings in [&b"prop"[..], &[0xff, 0], &[0]] {
        let bytes = blob(&words(&[1, 0, 3, 0, 0, 2, 9]), strings);
        assert_eq!(LinuxFdt::new(&bytes).err(), Some(FdtError::ParseError));
    }
    let bytes = blob(&words(&[1, 0, 3, 0, 1, 2, 9]), &[0xc3, 0xa9, 0]);
    assert_eq!(LinuxFdt::new(&bytes).err(), Some(FdtError::ParseError));
}

#[test]
fn block_ranges_alignment_versions_and_reservations_are_checked() {
    for (field, value) in [
        (1, 0),
        (1, 39),
        (2, 0),
        (2, 57),
        (2, u32::MAX),
        (3, 0),
        (3, 56),
        (3, u32::MAX),
        (4, 0),
        (4, 41),
        (4, 56),
        (4, u32::MAX),
        (5, 16),
        (6, 18),
        (8, u32::MAX),
        (9, 1),
        (9, u32::MAX),
    ] {
        let mut bytes = valid_blob();
        set_header(&mut bytes, field, value);
        assert_eq!(
            LinuxFdt::new(&bytes).err(),
            Some(FdtError::ParseError),
            "field {field}={value}"
        );
    }
    let mut bytes = valid_blob();
    bytes[47] = 1;
    assert_eq!(LinuxFdt::new(&bytes).err(), Some(FdtError::ParseError));
    let bytes = valid_blob();
    for len in 0..bytes.len() {
        assert_eq!(
            LinuxFdt::new(&bytes[..len]).err(),
            Some(FdtError::BufferTooSmall)
        );
    }
}

#[test]
fn nesting_limit_is_checked_before_iterators_are_exposed() {
    for depth in [63, 64] {
        let mut structure = words(&[1, 0]);
        for _ in 1..depth {
            structure.extend_from_slice(&words(&[1, 0x78000000]));
        }
        for _ in 0..depth {
            structure.extend_from_slice(&words(&[2]));
        }
        structure.extend_from_slice(&words(&[9]));
        let bytes = blob(&structure, b"");
        if depth == 63 {
            let fdt = LinuxFdt::new(&bytes).unwrap();
            assert_eq!(fdt.all_nodes().count(), depth);
            assert!(fdt.find_node("/absent").is_none());
        } else {
            assert_eq!(LinuxFdt::new(&bytes).err(), Some(FdtError::ParseError));
        }
    }
}

#[test]
fn valid_nodes_properties_absence_and_reservation_end_remain_distinct() {
    let bytes = valid_blob();
    let fdt = LinuxFdt::new(&bytes).unwrap();
    assert_eq!(fdt.all_nodes().count(), 2);
    assert_eq!(
        fdt.find_node("/").unwrap().property("prop").unwrap().value,
        &[0xff]
    );
    assert!(fdt.find_node("/child").is_some());
    assert!(fdt.find_node("/absent").is_none());
    let mut reservations = fdt.mem_reservations();
    assert!(reservations.next().is_none());
    assert!(reservations.next().is_none());
}

#[test]
fn padding_contents_and_property_name_suffix_sharing_remain_supported() {
    let mut bytes = valid_blob();
    bytes[56 + 21] = 0x80;
    assert!(LinuxFdt::new(&bytes).is_ok());
    let bytes = blob(&words(&[4, 1, 0, 4, 3, 0, 1, 4, 2, 4, 9]), b"xprop\0");
    let fdt = LinuxFdt::new(&bytes).unwrap();
    assert!(fdt.find_node("/").unwrap().property("prop").is_some());
}

#[test]
fn nops_are_accepted_at_every_token_boundary_before_end() {
    // Each element is one complete token and its payload, so inserting NOPs
    // exercises boundaries without changing names, property data, or padding.
    let records: &[&[u32]] = &[
        &[1, 0],
        &[3, 1, 0, 0xff000000],
        &[1, 0x78000000],
        &[3, 0, 0],
        &[2],
        &[1, 0x79000000],
        &[2],
        &[2],
        &[9],
    ];
    for boundary in 0..records.len() {
        for nop_count in [1, 3] {
            let mut structure = Vec::new();
            for (index, record) in records.iter().enumerate() {
                if index == boundary {
                    for _ in 0..nop_count {
                        structure.extend_from_slice(&words(&[4]));
                    }
                }
                structure.extend_from_slice(&words(record));
            }
            let bytes = blob(&structure, b"prop\0");
            let fdt = LinuxFdt::new(&bytes).unwrap();
            assert_eq!(fdt.all_nodes().count(), 3);
            assert_eq!(
                fdt.find_node("/").unwrap().property("prop").unwrap().value,
                &[0xff]
            );
            assert!(fdt.find_node("/x").unwrap().property("prop").is_some());
            assert!(fdt.find_node("/y").is_some());
        }
    }
}

#[test]
fn nops_do_not_hide_missing_root_unbalanced_nodes_or_trailing_tokens() {
    for tokens in [
        &[4, 4][..],
        &[4, 9],
        &[4, 2, 9],
        &[4, 3, 0, 0, 9],
        &[1, 0, 4, 9],
        &[1, 0, 1, 0x78000000, 2, 4, 9],
        &[1, 0, 1, 0x78000000, 2, 4, 3, 0, 0, 2, 9],
        &[1, 0, 2, 4],
        &[1, 0, 2, 4, 2, 9],
        &[1, 0, 2, 4, 1, 0, 2, 9],
        &[1, 0, 2, 4, 3, 0, 0, 9],
        &[1, 0, 2, 4, 5, 9],
        &[1, 0, 2, 4, 9, 4],
        &[1, 0, 2, 4, 9, 9],
    ] {
        let bytes = blob(&words(tokens), b"prop\0");
        assert_eq!(
            LinuxFdt::new(&bytes).err(),
            Some(FdtError::ParseError),
            "{tokens:x?}"
        );
    }
}

#[test]
fn pointer_entry_runs_the_same_structural_validation() {
    let mut bytes = valid_blob();
    bytes[56..60].copy_from_slice(&5u32.to_be_bytes());
    // SAFETY: the owned immutable buffer contains the full fixed header and
    // header-declared extent for the duration of this call.
    let result = unsafe { LinuxFdt::from_ptr(bytes.as_ptr()) };
    assert_eq!(result.err(), Some(FdtError::ParseError));
}

#[test]
fn accepted_single_byte_mutations_are_safe_to_traverse() {
    let original = valid_blob();
    for index in 0..original.len() {
        for mask in [1, 0x80, 0xff] {
            let mut bytes = original.clone();
            bytes[index] ^= mask;
            if let Ok(fdt) = LinuxFdt::new(&bytes) {
                for node in fdt.all_nodes() {
                    for property in node.properties() {
                        let _ = property.value.len();
                    }
                    let _ = node.parent_property("prop");
                }
                let _ = fdt.find_node("/child");
                let _ = fdt.find_node("/absent");
                let _ = fdt.mem_reservations().count();
            }
        }
    }
}
