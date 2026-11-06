// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use rs_fdtree::{LinuxFdt, PropertyError};

#[test]
fn availability_distinguishes_disabled_missing_and_malformed_status() {
    for (value, expected) in [
        (None, Ok(true)),
        (Some(b"okay\0".as_slice()), Ok(true)),
        (Some(b"ok\0".as_slice()), Ok(true)),
        (Some(b"disabled\0".as_slice()), Ok(false)),
        (Some(b"reserved\0".as_slice()), Ok(false)),
        (Some(b"fail-test\0".as_slice()), Ok(false)),
        (Some(b"unknown\0".as_slice()), Ok(false)),
        (Some(b"\0".as_slice()), Ok(false)),
        (Some(b"".as_slice()), Err(PropertyError::InvalidEncoding)),
        (
            Some(b"okay".as_slice()),
            Err(PropertyError::InvalidEncoding),
        ),
        (
            Some(b"\xff\0".as_slice()),
            Err(PropertyError::InvalidEncoding),
        ),
        (
            Some(b"okay\0disabled\0".as_slice()),
            Err(PropertyError::WrongType),
        ),
    ] {
        let properties = value
            .map(|value| vec![("status", value.to_vec())])
            .unwrap_or_default();
        let bytes = blob(&[properties]);
        let tree = LinuxFdt::new(&bytes).unwrap();
        assert_eq!(tree.all_nodes().nth(1).unwrap().is_available(), expected);
    }
    let bytes = blob(&[vec![
        ("status", b"okay\0".to_vec()),
        ("status", b"disabled\0".to_vec()),
    ]]);
    let tree = LinuxFdt::new(&bytes).unwrap();
    assert_eq!(
        tree.all_nodes().nth(1).unwrap().is_available(),
        Err(PropertyError::Duplicate)
    );
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
