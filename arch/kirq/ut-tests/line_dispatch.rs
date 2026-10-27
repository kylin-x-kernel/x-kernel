// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use unittest::def_test;

use super::IrqDispatchGuard;
use crate::{IrqDesc, state::{IRQ_STATE, IrqLine, IrqStateDesc}};

#[def_test(serial)]
fn dispatch_guard_finishes_original_line_not_replacement() {
    let virq = 0x5f34;
    let original = Arc::new(IrqLine::new(IrqStateDesc::new(IrqDesc::from_virq(virq))));
    let replacement = Arc::new(IrqLine::new(IrqStateDesc::new(IrqDesc::from_virq(virq))));
    original.lock().begin_dispatch();
    let guard = IrqDispatchGuard::new(virq, original.clone());

    // Deliberately bypass the production zero-in-flight removal gate: this
    // fixture tests the guard's ownership independently of directory policy.
    assert!(IRQ_STATE.lock().descs.insert(virq, replacement.clone()).is_none());
    drop(guard);

    let stored = IRQ_STATE.lock().descs.remove(&virq);
    assert!(stored.as_ref().is_some_and(|line| Arc::ptr_eq(line, &replacement)));
    assert_eq!(original.lock().in_flight(), 0);
    assert!(original.lock().in_flight_zero_completion().is_completed());
    assert_eq!(replacement.lock().in_flight(), 0);
}

#[def_test(serial)]
fn dispatch_guard_retains_line_until_completion() {
    let line = Arc::new(IrqLine::new(IrqStateDesc::new(IrqDesc::from_virq(0x5f35))));
    line.lock().begin_dispatch();
    let completion = line.lock().in_flight_zero_completion();
    let weak = Arc::downgrade(&line);
    let guard = IrqDispatchGuard::new(0x5f35, line.clone());
    drop(line);

    assert!(weak.upgrade().is_some());
    assert!(!completion.is_completed());
    drop(guard);
    assert!(completion.is_completed());
    assert!(weak.upgrade().is_none());
}
