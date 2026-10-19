// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::{
    HardIrqContextGuard, InterruptContextLevel, SoftIrqContextGuard, clear_irq_context_diagnostics,
    interrupt_context_level, irq_context_diagnostics, irq_context_snapshot, is_bh_disabled,
    is_in_hardirq, is_in_interrupt_context, is_serving_softirq, local_bh_disable,
};

#[def_test(serial)]
fn test_irq_context_snapshot_empty_by_default() {
    clear_irq_context_diagnostics();
    let snapshot = irq_context_snapshot();
    assert_eq!(snapshot.hardirq_depth(), 0);
    assert_eq!(snapshot.softirq_depth(), 0);
    assert_eq!(snapshot.bh_disable_depth(), 0);
    assert_eq!(interrupt_context_level(), InterruptContextLevel::Task);
    assert!(!is_in_interrupt_context());
}

#[def_test(serial)]
fn test_hardirq_context_guard_sets_and_restores_depth() {
    clear_irq_context_diagnostics();
    let _irq_guard = kspin::NoPreemptIrqSave::new();
    {
        let _guard = HardIrqContextGuard::enter();
        assert!(is_in_hardirq());
        assert!(is_in_interrupt_context());
        assert_eq!(interrupt_context_level(), InterruptContextLevel::Hardirq);
        assert_eq!(irq_context_snapshot().hardirq_depth(), 1);
    }
    assert!(!is_in_hardirq());
    assert!(!is_in_interrupt_context());
    assert_eq!(irq_context_diagnostics().context_underflow_warnings, 0);
}

#[def_test(serial)]
fn test_hardirq_context_guard_supports_nesting() {
    clear_irq_context_diagnostics();
    let _irq_guard = kspin::NoPreemptIrqSave::new();
    {
        let _outer = HardIrqContextGuard::enter();
        {
            let _inner = HardIrqContextGuard::enter();
            assert_eq!(irq_context_snapshot().hardirq_depth(), 2);
        }
        assert_eq!(irq_context_snapshot().hardirq_depth(), 1);
    }
    assert_eq!(irq_context_snapshot().hardirq_depth(), 0);
}

#[def_test(serial)]
fn test_synthetic_hardirq_masks_irqs_and_restores_context() {
    let before = irq_context_snapshot();
    let was_enabled = karch::local_irq_enabled();
    {
        let _irq = super::test_support::ScopedHardIrqContext::enter();
        assert!(!karch::local_irq_enabled());
        assert!(is_in_hardirq());
    }
    assert_eq!(irq_context_snapshot(), before);
    assert_eq!(karch::local_irq_enabled(), was_enabled);
}

#[def_test(serial)]
fn test_softirq_context_guard_sets_serving_state() {
    clear_irq_context_diagnostics();
    {
        let _guard = SoftIrqContextGuard::enter();
        assert!(is_serving_softirq());
        assert!(is_in_interrupt_context());
        assert_eq!(interrupt_context_level(), InterruptContextLevel::Softirq);
    }
    assert!(!is_serving_softirq());
}

#[def_test(serial)]
fn test_hardirq_level_takes_precedence_over_softirq() {
    clear_irq_context_diagnostics();
    let _irq_guard = kspin::NoPreemptIrqSave::new();
    assert!(!super::is_softirq_context());
    {
        let _softirq = SoftIrqContextGuard::enter();
        assert!(super::is_softirq_context());
        {
            let _hardirq = HardIrqContextGuard::enter();
            assert!(is_serving_softirq());
            assert!(!super::is_softirq_context());
            assert_eq!(interrupt_context_level(), InterruptContextLevel::Hardirq);
        }
        assert!(super::is_softirq_context());
    }
    assert!(!super::is_softirq_context());
}

#[def_test(serial)]
fn test_local_bh_guard_sets_and_restores_depth() {
    clear_irq_context_diagnostics();
    {
        let _guard = local_bh_disable();
        assert!(is_bh_disabled());
        assert!(is_in_interrupt_context());
        assert_eq!(interrupt_context_level(), InterruptContextLevel::BhDisabled);
        assert_eq!(irq_context_snapshot().bh_disable_depth(), 1);
    }
    assert!(!is_bh_disabled());
    assert!(!is_in_interrupt_context());
}

#[def_test]
fn test_nmi_classification_does_not_take_interrupted_context_lock() {
    let pin = kspin::NoPreemptIrqSave::new();
    let state = super::IRQ_CONTEXT_STATE.get(&pin).lock();
    // SAFETY: synthetic NMI on this pinned, IRQ-masked CPU. The queries
    // must bypass the ordinary context lock held by the interrupted code.
    let observed = unsafe {
        crate::runtime::rcu::with_nmi_context(|| {
            (
                interrupt_context_level(),
                is_in_interrupt_context(),
                super::is_softirq_context(),
            )
        })
    };
    drop(state);
    assert_eq!(observed, (InterruptContextLevel::Nmi, true, false));
    assert!(!super::is_in_nmi());
}

#[def_test(serial)]
fn test_softirq_level_takes_precedence_over_bh_disabled() {
    clear_irq_context_diagnostics();
    let _bh = local_bh_disable();
    let _softirq = SoftIrqContextGuard::enter();
    assert_eq!(interrupt_context_level(), InterruptContextLevel::Softirq);
}

#[def_test(serial)]
fn test_nested_local_bh_guard_restores_outer_depth() {
    clear_irq_context_diagnostics();
    {
        let _outer = local_bh_disable();
        {
            let _inner = local_bh_disable();
            assert_eq!(irq_context_snapshot().bh_disable_depth(), 2);
        }
        assert_eq!(irq_context_snapshot().bh_disable_depth(), 1);
    }
    assert_eq!(irq_context_snapshot().bh_disable_depth(), 0);
}
