// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Interrupt management.

use core::sync::atomic::{AtomicUsize, Ordering};

use kcpu::excp::{IRQ, register_trap_handler};
#[cfg(feature = "ipi")]
pub use kplat::interrupts::{TargetCpu, notify_cpu};
pub use kplat::interrupts::{
    dispatch_irq, enable, reg_handler as register, restore, save_disable, set_prio,
    unreg_handler as unregister,
};
#[cfg(feature = "ipi")]
pub use platconfig::devices::IPI_IRQ;

static IRQ_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Register a hook function called after an IRQ is dispatched.
///
/// This function can be called only once; subsequent calls will return false.
///
/// TODO: design a better api!
pub fn register_irq_hook(hook: fn(usize)) -> bool {
    IRQ_HOOK
        .compare_exchange(
            0,
            hook as *const () as usize,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .is_ok()
}

/// IRQ handler.
///
/// # Warn
///
/// Make sure called in an interrupt context or hypervisor VM exit handler.
#[register_trap_handler(IRQ)]
pub fn irq_handler(vector: usize) -> bool {
    let guard = kspin::NoPreempt::new();

    if let Some(irq) = dispatch_irq(vector) {
        let hook = IRQ_HOOK.load(Ordering::SeqCst);
        if hook != 0 {
            let hook = unsafe { core::mem::transmute::<usize, fn(usize)>(hook) };
            hook(irq);
        }
    }

    let _ = guard; // rescheduling may occur when preemption is re-enabled.
    true
}

#[cfg(feature = "khal_test")]
#[allow(missing_docs)]
pub mod tests_irq {
    use unittest::{
        test_fn, test_framework::TestDescriptor, test_framework_basic::TestResult, tests_name,
    };

    use super::{irq_handler, register_irq_hook};

    fn dummy_hook(_irq: usize) {}

    test_fn! {
        using TestResult;

        fn test_register_irq_hook_once() {
            let first = register_irq_hook(dummy_hook);
            let second = register_irq_hook(dummy_hook);
            assert!(!second);
            let _ = first;
        }
    }

    test_fn! {
        using TestResult;

        fn test_irq_handler_returns_true() {
            assert!(irq_handler(0));
        }
    }

    tests_name! {
        TEST_IRQ;
        test_register_irq_hook_once,
        test_irq_handler_returns_true,
    }
}
