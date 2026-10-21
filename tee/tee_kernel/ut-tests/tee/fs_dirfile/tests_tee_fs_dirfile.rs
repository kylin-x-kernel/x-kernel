// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq};

use super::*;

#[unittest::def_test]
fn test_fileh_some_zero_filenumber() {
    let dfh = TeeFsDirfileFileh {
        file_number: 0,
        hash: [0; TEE_FS_HTREE_HASH_SIZE],
        idx: 0,
    };
    let mut buffer = [0u8; 2];
    let result = tee_fs_dirfile_fileh_to_fname(Some(&dfh), &mut buffer);

    assert!(result.is_ok());
    let written_len = result.unwrap();
    assert_eq!(written_len, 1);
    assert_eq!(str::from_utf8(&buffer[..1]).unwrap(), "0");
}

#[unittest::def_test]
fn test_fileh_some_small_filenumber() {
    let dfh = TeeFsDirfileFileh {
        file_number: 0xABCD,
        hash: [0; TEE_FS_HTREE_HASH_SIZE],
        idx: 0,
    };
    let mut buffer = [0u8; 4];
    let result = tee_fs_dirfile_fileh_to_fname(Some(&dfh), &mut buffer);

    assert!(result.is_ok());
    let written_len = result.unwrap();
    assert_eq!(written_len, 4);
    assert_eq!(str::from_utf8(&buffer[..4]).unwrap(), "abcd");
}

#[unittest::def_test]
fn test_fileh_some_large_filenumber() {
    let dfh = TeeFsDirfileFileh {
        file_number: 0xFFFFFFFF,
        hash: [0; TEE_FS_HTREE_HASH_SIZE],
        idx: 0,
    };
    let mut buffer = [0u8; 8];
    let result = tee_fs_dirfile_fileh_to_fname(Some(&dfh), &mut buffer);

    assert!(result.is_ok());
    let written_len = result.unwrap();
    assert_eq!(written_len, 8);
    assert_eq!(str::from_utf8(&buffer[..8]).unwrap(), "ffffffff");
}

#[unittest::def_test]
fn test_fileh_none_case() {
    let mut buffer = [0u8; 7];
    let result = tee_fs_dirfile_fileh_to_fname(None, &mut buffer);

    assert!(result.is_ok());
    let written_len = result.unwrap();
    assert_eq!(written_len, 7);
    assert_eq!(str::from_utf8(&buffer[..7]).unwrap(), "dirf.db");
}

#[unittest::def_test]
fn test_fileh_short_buffer_file_number() {
    let dfh = TeeFsDirfileFileh {
        file_number: 0x1234,
        hash: [0; TEE_FS_HTREE_HASH_SIZE],
        idx: 0,
    };
    let mut buffer = [0u8; 3];
    let result = tee_fs_dirfile_fileh_to_fname(Some(&dfh), &mut buffer);

    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), TEE_ERROR_SHORT_BUFFER);
}

#[unittest::def_test]
fn test_fileh_short_buffer_dirf_db() {
    let mut buffer = [0u8; 6];
    let result = tee_fs_dirfile_fileh_to_fname(None, &mut buffer);

    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), TEE_ERROR_SHORT_BUFFER);
}

#[unittest::def_test]
fn test_fileh_empty_buffer() {
    let dfh = TeeFsDirfileFileh {
        file_number: 0x1234,
        hash: [0; TEE_FS_HTREE_HASH_SIZE],
        idx: 0,
    };
    let mut buffer = [0u8; 0];
    let result = tee_fs_dirfile_fileh_to_fname(Some(&dfh), &mut buffer);

    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), TEE_ERROR_SHORT_BUFFER);
}

#[unittest::def_test]
fn test_fileh_exact_buffer_file_number() {
    let dfh = TeeFsDirfileFileh {
        file_number: 0xABCDEF,
        hash: [0; TEE_FS_HTREE_HASH_SIZE],
        idx: 0,
    };
    let mut buffer = [0u8; 7];
    let result = tee_fs_dirfile_fileh_to_fname(Some(&dfh), &mut buffer);

    assert!(result.is_ok());
    let written_len = result.unwrap();
    assert_eq!(written_len, 6);
    assert_eq!(str::from_utf8(&buffer[..6]).unwrap(), "abcdef");
}

#[unittest::def_test]
fn test_fileh_exact_buffer_dirf_db() {
    let mut buffer = [0u8; 7];
    let result = tee_fs_dirfile_fileh_to_fname(None, &mut buffer);

    assert!(result.is_ok());
    let written_len = result.unwrap();
    assert_eq!(written_len, 7);
    assert_eq!(str::from_utf8(&buffer[..7]).unwrap(), "dirf.db");
}
