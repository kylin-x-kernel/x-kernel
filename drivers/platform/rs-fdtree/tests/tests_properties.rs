// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use rs_fdtree::{LinuxFdt, NodeProperty, PropertyError};

fn property(value: &[u8]) -> NodeProperty<'_> {
    NodeProperty {
        name: "test",
        value,
    }
}

fn cells(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_be_bytes())
        .collect()
}

fn blob(nodes: &[Vec<(&str, Vec<u8>)>]) -> Vec<u8> {
    let mut structure = cells(&[1, 0]);
    let mut strings = Vec::new();
    for properties in nodes {
        structure.extend(cells(&[1, 0x6e6f6465, 0]));
        for (name, value) in properties {
            structure.extend(cells(&[3, value.len() as u32, strings.len() as u32]));
            strings.extend_from_slice(name.as_bytes());
            strings.push(0);
            structure.extend_from_slice(value);
            while !structure.len().is_multiple_of(4) {
                structure.push(0);
            }
        }
        structure.extend(cells(&[2]));
    }
    structure.extend(cells(&[2, 9]));
    let strings_offset = 56 + structure.len();
    let mut bytes = cells(&[
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
    bytes.extend(structure);
    bytes.extend(strings);
    bytes
}

#[test]
fn strings_validate_the_entire_payload_before_iteration() {
    for bytes in [
        b"".as_slice(),
        b"missing-nul",
        b"good\0bad",
        b"good\0\xff\0",
    ] {
        assert!(matches!(
            property(bytes).read_strings(),
            Err(PropertyError::InvalidEncoding)
        ));
        assert_eq!(
            property(bytes).read_string(),
            Err(PropertyError::InvalidEncoding)
        );
    }
    assert_eq!(property(b"\0").read_string(), Ok(""));
    assert_eq!(
        property(b"first\0second\0").read_string(),
        Err(PropertyError::WrongType)
    );
    let names: Vec<_> = property(b"first\0\0last\0")
        .read_strings()
        .unwrap()
        .collect();
    assert_eq!(names, ["first", "", "last"]);
}

#[test]
fn scalar_cells_never_accept_a_prefix() {
    assert_eq!(property(&cells(&[7])).read_u32(), Ok(7));
    for values in [vec![], cells(&[7, 8])] {
        assert_eq!(property(&values).read_u32(), Err(PropertyError::WrongType));
    }
    for values in [vec![0; 3], vec![0; 5]] {
        assert_eq!(
            property(&values).read_u32(),
            Err(PropertyError::InvalidEncoding)
        );
    }
}

#[test]
fn array_errors_do_not_modify_output() {
    let mut output = [0xa5; 3];
    assert_eq!(
        property(&[0; 5]).read_u32_array(&mut output),
        Err(PropertyError::InvalidEncoding)
    );
    assert_eq!(output, [0xa5; 3]);
    assert_eq!(
        property(&cells(&[1, 2, 3, 4])).read_u32_array(&mut output),
        Err(PropertyError::BufferTooSmall { required: 4 })
    );
    assert_eq!(output, [0xa5; 3]);
    assert_eq!(property(&cells(&[1, 2])).read_u32_array(&mut output), Ok(2));
    assert_eq!(output, [1, 2, 0xa5]);
    assert_eq!(property(&[]).read_u32_array(&mut output), Ok(0));
    assert_eq!(output, [1, 2, 0xa5]);
}

#[test]
fn node_readers_distinguish_duplicate_and_missing_properties() {
    let bytes = blob(&[vec![
        ("duplicate", cells(&[1])),
        ("duplicate", cells(&[2])),
        ("single", b"name\0".to_vec()),
        ("list", b"first\0last\0".to_vec()),
        ("numbers", cells(&[7, 8])),
    ]]);
    let tree = LinuxFdt::new(&bytes).unwrap();
    let node = tree.all_nodes().nth(1).unwrap();
    assert_eq!(node.read_u32("missing"), Err(PropertyError::Missing));
    assert_eq!(node.read_u32("duplicate"), Err(PropertyError::Duplicate));
    assert_eq!(node.read_string("single"), Ok("name"));
    assert_eq!(
        node.read_strings("list").unwrap().collect::<Vec<_>>(),
        ["first", "last"]
    );
    let mut output = [99; 2];
    assert_eq!(
        node.read_u32_array("duplicate", &mut output),
        Err(PropertyError::Duplicate)
    );
    assert_eq!(output, [99; 2]);
    assert_eq!(node.read_u32_array("numbers", &mut output), Ok(2));
    assert_eq!(output, [7, 8]);
}
