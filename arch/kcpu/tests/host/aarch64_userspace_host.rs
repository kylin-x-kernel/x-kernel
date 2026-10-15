// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use aarch64_cpu::registers::ESR_EL1;

use super::{ExceptionKind, exception_kind_from_ec};

#[test]
fn classifies_instruction_traps_as_illegal_instruction() {
    assert_eq!(
        exception_kind_from_ec(Some(ESR_EL1::EC::Value::TrappedFP)),
        ExceptionKind::IllegalInstruction
    );
    assert_eq!(
        exception_kind_from_ec(Some(ESR_EL1::EC::Value::TrappedMsrMrs)),
        ExceptionKind::IllegalInstruction
    );
    assert_eq!(
        exception_kind_from_ec(Some(ESR_EL1::EC::Value::TrappedSve)),
        ExceptionKind::IllegalInstruction
    );
}

#[test]
fn keeps_breakpoint_and_alignment_classes_distinct() {
    assert_eq!(
        exception_kind_from_ec(Some(ESR_EL1::EC::Value::Brk64)),
        ExceptionKind::Breakpoint
    );
    assert_eq!(
        exception_kind_from_ec(Some(ESR_EL1::EC::Value::PCAlignmentFault)),
        ExceptionKind::Misaligned
    );
    assert_eq!(
        exception_kind_from_ec(Some(ESR_EL1::EC::Value::TrappedWFIorWFE)),
        ExceptionKind::Other
    );
}
