// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq};

use super::*;

const NODE_SIZE_TEST: usize = size_of::<super::TeeFsHtreeNodeImage>(); // 66
const HTREE_IMAGE_SIZE_TEST: usize = size_of::<super::TeeFsHtreeImage>(); // 256
const BLOCK_NODES_TEST: usize = BLOCK_SIZE / (NODE_SIZE_TEST * 2); // 4096 / (66 * 2) = 31

#[unittest::def_test]
fn test_get_offs_size_head() {
    let result = get_offs_size(TeeFsHtreeType::Head, 0, 0);
    assert_eq!(result.unwrap(), (0_usize, HTREE_IMAGE_SIZE_TEST));

    let result = get_offs_size(TeeFsHtreeType::Head, 0, 1);
    assert_eq!(
        result.unwrap(),
        (HTREE_IMAGE_SIZE_TEST, HTREE_IMAGE_SIZE_TEST)
    );

    let result = get_offs_size(TeeFsHtreeType::Head, 100, 0);
    assert_eq!(result.unwrap(), (0, HTREE_IMAGE_SIZE_TEST));
}

#[unittest::def_test]
fn test_get_offs_size_node() {
    let result = get_offs_size(TeeFsHtreeType::Node, 0, 0);
    assert_eq!(result.unwrap(), (4096, NODE_SIZE_TEST));
    let result = get_offs_size(TeeFsHtreeType::Node, 0, 1);
    assert_eq!(result.unwrap(), (4162, NODE_SIZE_TEST));
    let result = get_offs_size(TeeFsHtreeType::Node, 30, 0);
    assert_eq!(result.unwrap(), (8056, NODE_SIZE_TEST));
    let result = get_offs_size(TeeFsHtreeType::Node, 30, 1);
    assert_eq!(result.unwrap(), (8122, NODE_SIZE_TEST));
    let result = get_offs_size(TeeFsHtreeType::Node, 31, 0);
    assert_eq!(result.unwrap(), (258048, NODE_SIZE_TEST));
    let result = get_offs_size(TeeFsHtreeType::Node, 31, 1);
    assert_eq!(result.unwrap(), (258114, NODE_SIZE_TEST));
}

#[unittest::def_test]
fn test_get_offs_size_block() {
    let _block_nodes_x2_minus_1 = BLOCK_NODES_TEST * 2 - 1;
    let result = get_offs_size(TeeFsHtreeType::Block, 0, 0);
    assert_eq!(result.unwrap(), (8192, BLOCK_SIZE));
    let result = get_offs_size(TeeFsHtreeType::Block, 0, 1);
    assert_eq!(result.unwrap(), (12288, BLOCK_SIZE));
    let result = get_offs_size(TeeFsHtreeType::Block, 30, 0);
    assert_eq!(result.unwrap(), (253952, BLOCK_SIZE));
    let result = get_offs_size(TeeFsHtreeType::Block, 30, 1);
    assert_eq!(result.unwrap(), (262144, BLOCK_SIZE));
    let result = get_offs_size(TeeFsHtreeType::Block, 31, 0);
    assert_eq!(result.unwrap(), (266240, BLOCK_SIZE));
    let result = get_offs_size(TeeFsHtreeType::Block, 31, 1);
    assert_eq!(result.unwrap(), (270336, BLOCK_SIZE));
}

#[unittest::def_test]
fn test_get_offs_size_unsupported_type() {
    let result = get_offs_size(TeeFsHtreeType::UnsupportedType, 0, 0);
    assert_eq!(result.unwrap_err(), TEE_ERROR_GENERIC);
}

#[unittest::def_test(user)]
fn test_ree_fs_primitive_operations() {
    let uuid = TEE_UUID {
        timeLow: 0x12345678,
        timeMid: 0x1234,
        timeHiAndVersion: 0x5678,
        clockSeqAndNode: [0x9a, 0xbc, 0xde, 0xf0, 0x12, 0x34, 0x56, 0x78],
    };

    let dfh = TeeFsDirfileFileh {
        file_number: 0x12345678,
        hash: [0x01; TEE_FS_HTREE_HASH_SIZE],
        idx: 1,
    };

    let mut hash = [0u8; TEE_FS_HTREE_HASH_SIZE];
    let mut fdp =
        ree_fs_open_primitive(Some(&uuid), true, Some(&mut hash), Some(&dfh)).unwrap();

    assert_eq!(fdp.uuid, uuid);
    assert_eq!(fdp.dfh.file_number, dfh.file_number);

    let test_data =
        b"Hello, TEE World! This is a test message for ree_fs_primitive operations.";
    let write_result = ree_fs_write_primitive(&mut fdp, 0, test_data, &[], test_data.len());
    assert!(write_result.is_ok());

    let mut read_buffer = vec![0u8; test_data.len()];
    let mut read_len = read_buffer.len();
    let read_result =
        ree_fs_read_primitive(&mut fdp, 0, &mut read_buffer, &mut [], &mut read_len);
    assert!(read_result.is_ok());
    assert_eq!(read_len, test_data.len());
    assert_eq!(&read_buffer[..read_len], test_data);

    let mut partial_buffer = vec![0u8; 10];
    let mut partial_len = partial_buffer.len();
    let partial_read_result =
        ree_fs_read_primitive(&mut fdp, 0, &mut partial_buffer, &mut [], &mut partial_len);
    assert!(partial_read_result.is_ok());
    assert_eq!(partial_len, 10);
    assert_eq!(&partial_buffer[..partial_len], &test_data[..10]);

    let mut mid_buffer = vec![0u8; 15];
    let mut mid_len = mid_buffer.len();
    let mid_read_result =
        ree_fs_read_primitive(&mut fdp, 10, &mut mid_buffer, &mut [], &mut mid_len);
    assert!(mid_read_result.is_ok());
    assert_eq!(mid_len, 15);
    assert_eq!(&mid_buffer[..mid_len], &test_data[10..25]);

    let additional_data = b" Additional data appended to the file.";
    let additional_write_result = ree_fs_write_primitive(
        &mut fdp,
        test_data.len(),
        additional_data,
        &[],
        additional_data.len(),
    );
    assert!(additional_write_result.is_ok());

    let total_len = test_data.len() + additional_data.len();
    let mut full_buffer = vec![0u8; total_len];
    let mut full_len = full_buffer.len();
    let full_read_result =
        ree_fs_read_primitive(&mut fdp, 0, &mut full_buffer, &mut [], &mut full_len);
    assert!(full_read_result.is_ok());
    assert_eq!(full_len, total_len);

    let mut expected_full_data = Vec::new();
    expected_full_data.extend_from_slice(test_data);
    expected_full_data.extend_from_slice(additional_data);
    assert_eq!(&full_buffer[..full_len], expected_full_data.as_slice());
}
