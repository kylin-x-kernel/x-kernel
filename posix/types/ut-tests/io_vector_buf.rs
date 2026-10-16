// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn test_io_vector_buf_default() {
    let buf = IoVectorBuf::default();
    assert_eq!(buf.iovs.len(), 0);
    assert_eq!(buf.len, 0);
}

#[def_test]
fn test_io_vector_buf_io_remaining() {
    let buf = IoVectorBuf {
        iovs: Vec::new(),
        len: 42,
    };
    let io = buf.into_io();
    assert_eq!(io.remaining(), 42);
    assert_eq!(io.remaining_mut(), 42);
}
