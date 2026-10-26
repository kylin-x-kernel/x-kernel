// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Runtime-owned context fixtures for RCU's host contract and public read scopes.
//! Controlled IRQ/NMI markers test classification, not hardware delivery.

use unittest::{assert, assert_eq, def_test};

#[def_test]
fn host_context_queries_distinguish_bh_exclusion_from_handlers() {
    assert!(!kiface_irq_context_api::InterruptContextIf::is_in_nmi());
    assert!(!kiface_irq_context_api::InterruptContextIf::is_in_interrupt_context());
    assert!(!kiface_irq_context_api::InterruptContextIf::is_in_interrupt_handler());
    assert!(karch::local_irq_enabled());
    assert!(!ktask::current_preemption_disabled());

    let irq = kspin::IrqSave::new();
    {
        let _bh = kirq::context::local_bh_disable();
        assert!(!kiface_irq_context_api::InterruptContextIf::is_in_nmi());
        assert!(kiface_irq_context_api::InterruptContextIf::is_in_interrupt_context());
        assert!(!kiface_irq_context_api::InterruptContextIf::is_in_interrupt_handler());
        assert!(!karch::local_irq_enabled());
    }
    {
        let _hardirq = kirq::context::test_support::ScopedHardIrqContext::enter();
        assert!(!kiface_irq_context_api::InterruptContextIf::is_in_nmi());
        assert!(kiface_irq_context_api::InterruptContextIf::is_in_interrupt_context());
        assert!(kiface_irq_context_api::InterruptContextIf::is_in_interrupt_handler());
        // SAFETY: synthetic hardirq retains initialized CPU/current with IRQs
        // masked; these queries neither schedule nor change nesting/watching.
        let recognized_nmi = unsafe {
            kirq::context::test_support::with_nmi_context(|| {
                kiface_irq_context_api::InterruptContextIf::is_in_nmi()
                    && kiface_irq_context_api::InterruptContextIf::is_in_interrupt_context()
                    && kiface_irq_context_api::InterruptContextIf::is_in_interrupt_handler()
            })
        };
        assert!(recognized_nmi);
        assert!(!kiface_irq_context_api::InterruptContextIf::is_in_nmi());
        assert!(!karch::local_irq_enabled());
    }
    assert!(!kiface_irq_context_api::InterruptContextIf::is_in_interrupt_context());
    assert!(!kiface_irq_context_api::InterruptContextIf::is_in_interrupt_handler());
    assert!(!ktask::current_preemption_disabled());
    assert!(!karch::local_irq_enabled());
    drop(irq);
    assert!(karch::local_irq_enabled());
}

#[def_test]
fn task_entry_restores_exclusion_before_nested_read_bodies() {
    let state_fn = || {
        (
            karch::local_irq_enabled(),
            ktask::current_preemption_disabled(),
        )
    };
    assert_eq!(state_fn(), (true, false));
    let read_state_fn = || {
        krcu::with_read(|_| {
            let outer = state_fn();
            let inner = krcu::with_read(|_| state_fn());
            (outer, inner, state_fn())
        })
    };
    assert_eq!(
        read_state_fn(),
        ((true, false), (true, false), (true, false))
    );
    {
        let _irq = kspin::IrqSave::new();
        assert_eq!(
            read_state_fn(),
            ((false, false), (false, false), (false, false))
        );
        assert_eq!(state_fn(), (false, false));
    }
    {
        let _guard = kspin::NoPreemptIrqSave::new();
        assert_eq!(
            read_state_fn(),
            ((false, true), (false, true), (false, true))
        );
        assert_eq!(state_fn(), (false, true));
    }
    assert_eq!(state_fn(), (true, false));
}

#[def_test]
fn exception_snapshot_preserves_exclusion_and_reports_active_frame() {
    let was_irq_enabled = karch::local_irq_enabled();
    let was_preempt_disabled = ktask::current_preemption_disabled();
    let mut calls = 0;
    let mut saw_masked_task = false;
    kiface_cpu_api::ExceptionContextIf::with_exception_snapshot(&mut |is_active| {
        calls += 1;
        saw_masked_task = !is_active
            && !karch::local_irq_enabled()
            && ktask::current_preemption_disabled() == was_preempt_disabled
            && kiface_cpu_api::CurrentCpuIf::current_cpu() == khal::percpu::this_cpu_id();
    });
    assert_eq!(calls, 1);
    assert!(saw_masked_task);
    assert_eq!(karch::local_irq_enabled(), was_irq_enabled);
    assert_eq!(ktask::current_preemption_disabled(), was_preempt_disabled);

    let irq = kspin::IrqSave::new();
    let frame = kcpu::ExceptionContext::default();
    {
        let _exception = kcpu::ExceptionContextGuard::new(&frame);
        assert!(kiface_cpu_api::ExceptionContextIf::is_in_exception_context());
        let mut saw_masked_exception = false;
        kiface_cpu_api::ExceptionContextIf::with_exception_snapshot(&mut |is_active| {
            calls += 1;
            saw_masked_exception = is_active
                && !karch::local_irq_enabled()
                && ktask::current_preemption_disabled() == was_preempt_disabled;
        });
        assert!(saw_masked_exception);
        assert!(!karch::local_irq_enabled());
    }
    assert!(!kiface_cpu_api::ExceptionContextIf::is_in_exception_context());
    assert!(!karch::local_irq_enabled());
    drop(irq);
    assert_eq!(calls, 2);
    assert_eq!(karch::local_irq_enabled(), was_irq_enabled);
    assert_eq!(ktask::current_preemption_disabled(), was_preempt_disabled);
}
