// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use khal::trap::PageFaultFlags;
use memaddr::VirtAddr;
use unittest::def_test;

use super::{FaultInput, FaultOutcome};

#[def_test]
fn fault_input_preserves_trap_address_and_access() {
    let input = FaultInput::new(VirtAddr::from_usize(0x4000), PageFaultFlags::WRITE);

    assert_eq!(input.address(), VirtAddr::from_usize(0x4000));
    assert_eq!(input.access_flags(), PageFaultFlags::WRITE);
    assert_eq!(input.into_context().address(), VirtAddr::from_usize(0x4000));
}

#[def_test]
fn fault_outcome_classifies_success_retry_and_bus() {
    assert!(FaultOutcome::Resolved.is_resolved());
    assert!(!FaultOutcome::Unmapped.is_resolved());
    assert!(FaultOutcome::Retry.is_retryable());
    assert!(FaultOutcome::CowConflictRetry.is_retryable());
    assert!(!FaultOutcome::AccessDenied.is_retryable());
    assert!(FaultOutcome::BusError.is_bus_error());
}
