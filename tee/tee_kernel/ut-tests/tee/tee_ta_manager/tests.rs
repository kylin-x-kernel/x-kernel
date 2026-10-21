// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::*;

#[unittest::def_test]
fn validated_payload_len_accepts_within_limit() {
    assert_eq!(
        validated_payload_len(TEE_TA_MAX_FRAME_PAYLOAD_LEN).unwrap(),
        TEE_TA_MAX_FRAME_PAYLOAD_LEN
    );
}

#[unittest::def_test]
fn validated_payload_len_rejects_oversized_frames() {
    assert_eq!(
        validated_payload_len(TEE_TA_MAX_FRAME_PAYLOAD_LEN + 1),
        Err(TEE_ERROR_GENERIC)
    );
}
