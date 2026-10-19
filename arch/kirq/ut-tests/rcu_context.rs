// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::{RcuContextIf, with_nmi_context};
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
