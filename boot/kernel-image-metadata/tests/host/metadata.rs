// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::*;

#[test]
fn round_trip_preserves_payload() {
    let mut descriptor = [0xa5; BUILD_INFO_DESCRIPTOR_SIZE];
    let expected = "arch = aarch64\nplatform = kplat-aarch64\n";
    let encoded_size = encode_build_info(expected, &mut descriptor).unwrap();
    assert_eq!(decode_build_info(&descriptor), Ok(expected));
    assert_eq!(encoded_size, HEADER_SIZE + expected.len());
    assert!(descriptor[encoded_size..].iter().all(|byte| *byte == 0));
}

#[test]
fn corrupted_payload_is_rejected() {
    let mut descriptor = [0; BUILD_INFO_DESCRIPTOR_SIZE];
    encode_build_info("arch = aarch64\n", &mut descriptor).unwrap();
    descriptor[HEADER_SIZE] ^= 1;
    assert_eq!(
        decode_build_info(&descriptor),
        Err(MetadataError::ChecksumMismatch)
    );
}

#[test]
fn oversized_payload_is_rejected() {
    let mut descriptor = [0; BUILD_INFO_DESCRIPTOR_SIZE];
    let payload = "x".repeat(BUILD_INFO_DESCRIPTOR_SIZE);
    assert_eq!(
        encode_build_info(&payload, &mut descriptor),
        Err(MetadataError::PayloadTooLarge)
    );
}
