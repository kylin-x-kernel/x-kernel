// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::cell::Cell;

use unittest::def_test;

#[def_test]
fn msi_retrigger_retains_message_target_until_allocation_release() {
    let mut pool = super::MsixVectorAllocator::new();
    let vector = pool.alloc().unwrap();
    assert!(!pool.retrigger(vector, |_, _| panic!("unbound allocation")));
    assert!(pool.bind_destination(vector, 7));
    assert!(pool.matches_destination(vector, None));
    assert!(pool.matches_destination(vector, Some(7)));
    assert!(!pool.matches_destination(vector, Some(3)));
    assert!(pool.bind_destination(vector, 7));
    assert!(!pool.bind_destination(vector, 3));
    let sent = Cell::new(None);
    assert!(pool.retrigger(vector, |vector, cpu| {
        sent.set(Some((vector, cpu)));
        true
    }));
    assert_eq!(sent.get(), Some((vector, 7)));
    assert!(pool.free(vector));
    assert!(!pool.matches_destination(vector, None));
    assert!(!pool.retrigger(vector, |_, _| panic!("freed allocation")));
    assert!(!pool.bind_destination(vector, 9));
    let reused = pool.alloc().unwrap();
    assert_eq!(reused, vector);
    assert!(!pool.retrigger(reused, |_, _| panic!("stale destination")));
    assert!(pool.bind_destination(reused, 9));
    assert!(pool.retrigger(reused, |vector, cpu| {
        sent.set(Some((vector, cpu)));
        true
    }));
    assert_eq!(sent.get(), Some((reused, 9)));
    assert!(pool.free(reused));
}

#[def_test]
fn msi_retrigger_reports_send_rejection_and_keeps_target_binding() {
    let mut pool = super::MsixVectorAllocator::new();
    let vector = pool.alloc().unwrap();
    assert!(pool.bind_destination(vector, 7));
    assert!(!pool.retrigger(vector, |_, _| false));
    assert!(pool.matches_destination(vector, Some(7)));
    assert!(pool.retrigger(vector, |_, destination| destination == 7));
    assert!(pool.free(vector));
}

#[def_test]
fn msi_route_operations_reject_other_vector_classes() {
    let mut pool = super::MsixVectorAllocator::new();
    for vector in [
        0,
        super::MSIX_VECTOR_BASE - 1,
        super::APIC_TIMER_VECTOR,
        255,
    ] {
        assert!(!pool.bind_destination(vector, 0));
        assert!(!pool.retrigger(vector, |_, _| panic!("invalid vector")));
        assert!(!pool.free(vector));
    }
}
