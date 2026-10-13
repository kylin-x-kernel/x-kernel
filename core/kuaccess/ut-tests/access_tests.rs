// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use memspace::PageFaultOutcome;
use osvm::MemError;
use unittest::def_test;
use unittest_support::TestUserValue;

use super::{
    USER_SPACE_BASE, USER_SPACE_SIZE, atomic_cmpxchg_u32, atomic_load_u32, atomic_u32_eq,
    check_access, fault_outcome_to_trap_result,
};

#[def_test]
fn test_check_access_valid() {
    assert!(check_access(USER_SPACE_BASE, 1).is_ok());
}

#[def_test]
fn test_check_access_invalid_low() {
    let res = check_access(USER_SPACE_BASE - 1, 1);
    assert!(matches!(res, Err(MemError::NoAccess)));
}

#[def_test]
fn test_check_access_invalid_len() {
    let res = check_access(USER_SPACE_BASE, USER_SPACE_SIZE + 1);
    assert!(matches!(res, Err(MemError::NoAccess)));
}

#[def_test]
fn test_check_access_zero_len_and_upper_boundary() {
    assert!(check_access(USER_SPACE_BASE, 0).is_ok());
    assert!(check_access(USER_SPACE_BASE + USER_SPACE_SIZE - 1, 1).is_ok());
    assert!(check_access(USER_SPACE_BASE + USER_SPACE_SIZE, 0).is_err());
}

#[def_test]
fn test_check_access_end_overflow_rejected() {
    let res = check_access(USER_SPACE_BASE + USER_SPACE_SIZE - 1, 2);
    assert!(matches!(res, Err(MemError::NoAccess)));
}

#[def_test]
fn test_atomic_cmpxchg_u32_rejects_misaligned() {
    let res = atomic_cmpxchg_u32(USER_SPACE_BASE + 1, 0, 1);
    assert!(matches!(res, Err(MemError::InvalidAddr)));
}

#[def_test]
fn test_atomic_cmpxchg_u32_rejects_out_of_range() {
    let res = atomic_cmpxchg_u32(USER_SPACE_BASE - 4, 0, 1);
    assert!(matches!(res, Err(MemError::NoAccess)));
}

#[def_test(user)]
fn test_atomic_cmpxchg_u32_match_updates_user_word() {
    let word = TestUserValue::<u32>::from_value(10).unwrap();
    let addr = word.as_user_ptr() as usize;

    let res = atomic_cmpxchg_u32(addr, 10, 20).unwrap();
    assert_eq!(res, (true, 10));
    assert_eq!(word.read(), 20);
}

#[def_test(user)]
fn test_atomic_cmpxchg_u32_mismatch_leaves_user_word() {
    let word = TestUserValue::<u32>::from_value(10).unwrap();
    let addr = word.as_user_ptr() as usize;

    let res = atomic_cmpxchg_u32(addr, 5, 20).unwrap();
    assert_eq!(res, (false, 10));
    assert_eq!(word.read(), 10);
}

#[def_test(user)]
fn test_atomic_load_u32_reads_user_word() {
    let word = TestUserValue::<u32>::from_value(42).unwrap();
    let addr = word.as_user_ptr() as usize;

    assert_eq!(atomic_load_u32(addr).unwrap(), 42);
}

#[def_test(user)]
fn test_atomic_u32_eq_matches_user_word() {
    let word = TestUserValue::<u32>::from_value(7).unwrap();
    let addr = word.as_user_ptr() as usize;

    assert!(atomic_u32_eq(addr, 7).unwrap());
    assert!(!atomic_u32_eq(addr, 8).unwrap());
}

#[def_test(user)]
fn test_atomic_cmpxchg_u32_faults_on_unmapped_user_addr() {
    let unmapped = USER_SPACE_BASE + 0x1000;
    let res = atomic_cmpxchg_u32(unmapped, 0, 1);
    assert!(matches!(res, Err(MemError::NoAccess)));
}

#[def_test]
fn test_check_access_rejects_far_above_user_space() {
    let res = check_access(USER_SPACE_BASE + USER_SPACE_SIZE + 0x1000, 1);
    assert!(matches!(res, Err(MemError::NoAccess)));
}

#[def_test]
fn fault_outcome_mapping_keeps_retry_inside_copy_window() {
    assert!(fault_outcome_to_trap_result(PageFaultOutcome::Resolved));
    assert!(fault_outcome_to_trap_result(PageFaultOutcome::Retry));
    assert!(fault_outcome_to_trap_result(
        PageFaultOutcome::CowConflictRetry
    ));
}

#[def_test]
fn fault_outcome_mapping_rejects_user_copy_failures() {
    assert!(!fault_outcome_to_trap_result(PageFaultOutcome::Unmapped));
    assert!(!fault_outcome_to_trap_result(
        PageFaultOutcome::AccessDenied
    ));
    assert!(!fault_outcome_to_trap_result(PageFaultOutcome::BusError));
    assert!(!fault_outcome_to_trap_result(PageFaultOutcome::OutOfMemory));
    assert!(!fault_outcome_to_trap_result(PageFaultOutcome::NoProgress));
    assert!(!fault_outcome_to_trap_result(PageFaultOutcome::Failed));
}
