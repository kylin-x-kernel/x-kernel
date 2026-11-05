// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::{RcuContextIf, with_nmi_claim, with_nmi_context};
use crate::context::{self, InterruptContextLevel};

#[def_test]
fn nested_nmi_scopes_keep_watching_and_restore_context() {
    let _pin = kspin::NoPreemptIrqSave::new();
    let cpu = RcuContextIf::current_cpu_watching();
    let before = cpu.interrupt_depth_for_test();
    assert!(!context::is_in_nmi());
    let observe_fn = || {
        let outer = (context::is_in_nmi(), cpu.interrupt_depth_for_test());
        // SAFETY: the same exclusion covers this strictly nested scope.
        let inner = unsafe {
            with_nmi_context(|| {
                (
                    context::interrupt_context_level(),
                    context::is_in_interrupt_context(),
                    context::is_softirq_context(),
                    cpu.interrupt_depth_for_test(),
                    cpu.is_watching(),
                )
            })
        };
        let restored = (context::is_in_nmi(), cpu.interrupt_depth_for_test());
        (outer, inner, restored)
    };
    // SAFETY: this synthetic entry runs under IRQ/preemption exclusion and
    // balances every nested scope without dispatching hardware or scheduling.
    let observed = unsafe { with_nmi_context(observe_fn) };
    assert_eq!(observed.0, (true, before + 1));
    assert_eq!(
        observed.1,
        (InterruptContextLevel::Nmi, true, false, before + 2, true)
    );
    assert_eq!(observed.2, observed.0);
    assert_eq!(cpu.interrupt_depth_for_test(), before);
    assert!(!context::is_in_nmi());
    assert!(cpu.is_watching());
}

#[def_test]
fn nmi_claim_keeps_read_scope_through_completion() {
    let _pin = kspin::NoPreemptIrqSave::new();
    let cpu = RcuContextIf::current_cpu_watching();
    let before = cpu.interrupt_depth_for_test();
    let stage = core::cell::Cell::new(0);
    let observations = core::cell::Cell::new((false, false));
    assert!(!krcu::is_reading());
    // SAFETY: synthetic claim, no hardware writes, and both callbacks remain
    // under the same CPU/IRQ exclusion without scheduling or escaping state.
    unsafe {
        with_nmi_claim(
            |_| {
                let valid = context::is_in_nmi()
                    && cpu.is_watching()
                    && krcu::is_reading()
                    && cpu.interrupt_depth_for_test() == before + 1;
                observations.set((valid, false));
                stage.set(1);
            },
            || {
                let valid = stage.get() == 1
                    && context::is_in_nmi()
                    && cpu.is_watching()
                    && krcu::is_reading()
                    && cpu.interrupt_depth_for_test() == before + 1;
                observations.set((observations.get().0, valid));
                stage.set(2);
            },
        );
    }
    assert_eq!(stage.get(), 2);
    assert_eq!(observations.get(), (true, true));
    assert!(!krcu::is_reading());
    assert!(!context::is_in_nmi());
    assert_eq!(cpu.interrupt_depth_for_test(), before);
}

#[def_test]
fn nested_nmi_claim_preserves_the_interrupted_reader() {
    let observed = krcu::with_read(|_| {
        let _pin = kspin::NoPreemptIrqSave::new();
        let completed = core::cell::Cell::new(0);
        let is_valid = core::cell::Cell::new(true);
        let dispatch_nested = |_: &krcu::ReadScope<'_>| {
            // SAFETY: the outer claim retains CPU/IRQ exclusion; this nested
            // synthetic claim finishes on the same stack without scheduling.
            unsafe {
                with_nmi_claim(
                    |_| is_valid.set(is_valid.get() && krcu::is_reading()),
                    || {
                        is_valid.set(is_valid.get() && krcu::is_reading());
                        completed.set(1);
                    },
                );
            }
            is_valid.set(is_valid.get() && krcu::is_reading());
        };
        // SAFETY: synthetic claims remain on this pinned stack, do not touch
        // hardware, and finish their completions before restoring the task.
        unsafe {
            with_nmi_claim(dispatch_nested, || {
                is_valid.set(is_valid.get() && completed.get() == 1 && krcu::is_reading());
                completed.set(2);
            });
        }
        (
            is_valid.get(),
            completed.get(),
            krcu::is_reading(),
            context::is_in_nmi(),
        )
    });
    assert_eq!(observed, (true, 2, true, false));
    assert!(!krcu::is_reading());
}
