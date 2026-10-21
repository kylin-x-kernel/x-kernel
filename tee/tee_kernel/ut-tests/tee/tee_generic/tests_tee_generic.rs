// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::assert_eq;

use super::*;

#[unittest::def_test(user)]
fn test_scn_log_normal_message() {
    let user_buf = crate::TestUserBuffer::new(5).unwrap();
    user_buf.write_bytes(b"loghi").unwrap();

    assert_eq!(sys_tee_scn_log(user_buf.as_user_ptr(), 5), Ok(()));
}

#[unittest::def_test(user)]
fn test_scn_log_rejects_oversized_len() {
    // No mapped user buffer is supplied on purpose: the length check has to
    // reject these requests before any allocation or user-memory access.
    assert_eq!(
        sys_tee_scn_log(core::ptr::null(), usize::MAX),
        Err(TEE_ERROR_EXCESS_DATA)
    );
    assert_eq!(
        sys_tee_scn_log(core::ptr::null(), VM_STRING_MAX_LEN + 1),
        Err(TEE_ERROR_EXCESS_DATA)
    );
}

#[unittest::def_test(user)]
fn test_scn_log_rejects_invalid_addr() {
    assert_eq!(
        sys_tee_scn_log(core::ptr::null(), 8),
        Err(TEE_ERROR_BAD_PARAMETERS)
    );
}

#[unittest::def_test(user)]
fn test_scn_log_rejects_non_utf8() {
    let user_buf = crate::TestUserBuffer::new(1).unwrap();
    user_buf.write_bytes(&[0xFF]).unwrap();

    assert_eq!(
        sys_tee_scn_log(user_buf.as_user_ptr(), 1),
        Err(TEE_ERROR_BAD_PARAMETERS)
    );
}
