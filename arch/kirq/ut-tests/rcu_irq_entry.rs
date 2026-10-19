// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::{RcuContextIf, handle_irq_with, handle_nmi_with, with_nmi_context};
use crate::context;

#[def_test]
fn dedicated_nmi_dispatch_balances_watching_without_a_claim() {
    let _pin = kspin::NoPreemptIrqSave::new();
    let cpu = RcuContextIf::current_cpu_watching();
    let before = cpu.interrupt_depth_for_test();
    let mut observed = None;
    let handled = handle_nmi_with(0, |_| {
        observed = Some((
            context::is_in_nmi(),
            cpu.interrupt_depth_for_test(),
            cpu.is_watching(),
        ));
    });
    assert!(handled);
    assert_eq!(observed, Some((true, before + 1, true)));
    assert_eq!(cpu.interrupt_depth_for_test(), before);
    assert!(!context::is_in_nmi());
    assert!(cpu.is_watching());
}

#[def_test(serial)]
fn spurious_irq_with_synthetic_nested_nmi_balances_watching() {
    let _pin = kspin::NoPreemptIrqSave::new();
    let cpu = RcuContextIf::current_cpu_watching();
    let before = cpu.interrupt_depth_for_test();
    let mut irq_observed = None;
    let mut nmi_observed = None;
    let handled = handle_irq_with(0, |_| {
        irq_observed = Some((
            context::is_in_hardirq(),
            context::is_in_nmi(),
            cpu.interrupt_depth_for_test(),
            cpu.is_watching(),
        ));
        // SAFETY: synthetic controller classification inside the IRQ entry;
        // the test's guard keeps this CPU pinned and ordinary IRQs masked.
        nmi_observed = Some(unsafe {
            with_nmi_context(|| (context::is_in_nmi(), cpu.interrupt_depth_for_test()))
        });
        None
    });
    assert!(handled);
    assert_eq!(irq_observed, Some((true, false, before + 1, true)));
    assert_eq!(nmi_observed, Some((true, before + 2)));
    assert_eq!(cpu.interrupt_depth_for_test(), before);
    assert!(!context::is_in_nmi());
    assert!(!context::is_in_hardirq());
    assert!(cpu.is_watching());
}
