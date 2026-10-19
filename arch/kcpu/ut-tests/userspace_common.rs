// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::{ExceptionKind, ReturnReason};

#[def_test]
fn test_exception_kind_equality() {
    assert_ne!(ExceptionKind::Breakpoint, ExceptionKind::Misaligned);
}

#[def_test]
fn test_exception_kind_variants_distinct() {
    assert_ne!(ExceptionKind::IllegalInstruction, ExceptionKind::Other);
    assert_ne!(ExceptionKind::Misaligned, ExceptionKind::Breakpoint);
}

#[def_test]
fn test_return_reason_match() {
    let reason = ReturnReason::Syscall;
    assert!(matches!(reason, ReturnReason::Syscall));
}
