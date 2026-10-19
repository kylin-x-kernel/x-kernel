// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn test_irq_slice_empty_by_default() {
    let count = IRQ.iter().count();
    assert_eq!(IRQ.len(), count);
}

#[def_test]
fn test_page_fault_slice_empty_by_default() {
    let count = PAGE_FAULT.iter().count();
    assert_eq!(PAGE_FAULT.len(), count);
}

#[def_test]
fn test_page_fault_flags_bits() {
    let flags = PageFaultFlags::empty();
    assert!(flags.is_empty());
}
