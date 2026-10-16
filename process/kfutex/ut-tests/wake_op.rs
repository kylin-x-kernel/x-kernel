// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use linux_raw_sys::general::{
    FUTEX_OP_ADD, FUTEX_OP_CMP_EQ, FUTEX_OP_CMP_LT, FUTEX_OP_OPARG_SHIFT, FUTEX_OP_XOR,
};
use unittest::def_test;

use super::FutexWakeOp;

fn encode(operation: u32, operation_argument: u32, comparison: u32, argument: u32) -> u32 {
    (operation << 28)
        | ((comparison & 0xf) << 24)
        | ((operation_argument & 0xfff) << 12)
        | (argument & 0xfff)
}

#[def_test]
fn wake_op_uses_signed_12_bit_arguments() {
    let operation = FutexWakeOp::decode(encode(FUTEX_OP_ADD, 0xfff, FUTEX_OP_CMP_LT, 0xfff))
        .expect("decode signed wake operation");
    assert_eq!(operation.apply(7), 6);
    assert!(!operation.compare(0));
    assert!(operation.compare(u32::MAX - 1));
}

#[def_test]
fn wake_op_shift_validates_argument() {
    let valid = encode(FUTEX_OP_XOR | FUTEX_OP_OPARG_SHIFT, 5, FUTEX_OP_CMP_EQ, 0);
    let operation = FutexWakeOp::decode(valid).expect("decode valid shift");
    assert_eq!(operation.apply(32), 0);

    let invalid = encode(FUTEX_OP_XOR | FUTEX_OP_OPARG_SHIFT, 32, FUTEX_OP_CMP_EQ, 0);
    assert!(FutexWakeOp::decode(invalid).is_err());
}
