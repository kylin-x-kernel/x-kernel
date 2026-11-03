// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::tee::control_protocol::{
    control_frame_payload_len, ControlFrameError, MAX_CONTROL_PAYLOAD,
};

#[unittest::def_test]
fn control_frame_payload_len_accepts_within_limit() {
    assert_eq!(
        control_frame_payload_len((MAX_CONTROL_PAYLOAD as u32).to_le_bytes()),
        Ok(MAX_CONTROL_PAYLOAD)
    );
}

#[unittest::def_test]
fn control_frame_payload_len_rejects_oversized_frames() {
    assert_eq!(
        control_frame_payload_len((MAX_CONTROL_PAYLOAD as u32 + 1).to_le_bytes()),
        Err(ControlFrameError::Oversize(MAX_CONTROL_PAYLOAD + 1))
    );
}
