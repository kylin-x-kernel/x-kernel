// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq};

use super::*;

#[unittest::def_test(user)]
fn test_load_string_with_len_exact_content() {
    let user_buf = crate::TestUserBuffer::new(5).unwrap();
    user_buf.write_bytes(b"hello").unwrap();

    let loaded = vm_load_string_with_len(user_buf.as_user_ptr(), 5).unwrap();
    assert_eq!(loaded, "hello");
}

#[unittest::def_test(user)]
fn test_load_string_with_len_rejects_over_limit_before_alloc() {
    // The null pointer is never dereferenced: the length check must reject
    // the request before any allocation or user-memory access happens.
    assert_eq!(
        vm_load_string_with_len(core::ptr::null(), usize::MAX),
        Err(KError::OutOfRange)
    );
    assert_eq!(
        vm_load_string_with_len(core::ptr::null(), VM_STRING_MAX_LEN + 1),
        Err(KError::OutOfRange)
    );
}

#[unittest::def_test(user)]
fn test_load_string_with_len_accepts_limit_boundary() {
    let filler = alloc::vec![b'a'; VM_STRING_MAX_LEN];
    let user_buf = crate::TestUserBuffer::new(VM_STRING_MAX_LEN).unwrap();
    user_buf.write_bytes(&filler).unwrap();

    let loaded = vm_load_string_with_len(user_buf.as_user_ptr(), VM_STRING_MAX_LEN).unwrap();
    assert_eq!(loaded.len(), VM_STRING_MAX_LEN);
    assert!(loaded.bytes().all(|byte| byte == b'a'));
}

#[unittest::def_test(user)]
fn test_load_string_with_len_rejects_invalid_addr() {
    assert_eq!(
        vm_load_string_with_len(core::ptr::null(), 8),
        Err(KError::BadAddress)
    );
}

#[unittest::def_test(user)]
fn test_load_string_with_len_rejects_invalid_utf8() {
    let user_buf = crate::TestUserBuffer::new(1).unwrap();
    user_buf.write_bytes(&[0xFF]).unwrap();

    assert_eq!(
        vm_load_string_with_len(user_buf.as_user_ptr(), 1),
        Err(KError::IllegalBytes)
    );
}
