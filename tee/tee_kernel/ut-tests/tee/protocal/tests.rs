// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec;

use super::{
    LEGACY_FRAME_PAYLOAD_LIMIT, LEGACY_PANIC_VARIANT_TAG, LegacyPanicFrame,
    validated_payload_len,
};

#[unittest::def_test]
fn legacy_panic_frame_golden_bytes() {
    // postcard:u32 varint = LEB128(无 zigzag);0x80 起多字节。
    assert_eq!(LegacyPanicFrame::new(0).encode().unwrap(), vec![0x04, 0x00]);
    assert_eq!(LegacyPanicFrame::new(1).encode().unwrap(), vec![0x04, 0x01]);
    assert_eq!(
        LegacyPanicFrame::new(300).encode().unwrap(),
        vec![0x04, 0xAC, 0x02]
    );
    assert_eq!(
        LegacyPanicFrame::new(0xFFFF_FFFF).encode().unwrap(),
        vec![0x04, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F]
    );
    let decoded: LegacyPanicFrame =
        postcard::from_bytes(&LegacyPanicFrame::new(77).encode().unwrap()).unwrap();
    assert_eq!(decoded.variant_tag, LEGACY_PANIC_VARIANT_TAG);
    assert_eq!(decoded.panic_code, 77);
}

#[unittest::def_test]
fn legacy_frame_payload_limit_enforced() {
    assert_eq!(
        validated_payload_len(LEGACY_FRAME_PAYLOAD_LIMIT).unwrap(),
        LEGACY_FRAME_PAYLOAD_LIMIT
    );
    assert!(validated_payload_len(LEGACY_FRAME_PAYLOAD_LIMIT + 1).is_err());
}
