// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ trap/preemption lifecycle hooks.

use core::sync::atomic::{AtomicBool, Ordering};

use kspin::SpinNoIrq;

/// Callback invoked by the IRQ core while normal preemption is disabled.
///
/// Hook implementations must be IRQ-context safe: they must not sleep, block on
/// locks that can be held by interrupted contexts, or rely on process context.
pub type IrqLifecycleHook = fn();

/// Optional IRQ trap/preemption lifecycle callbacks.
///
/// The hooks are intentionally minimal. They describe an IRQ-on exception
/// lifetime while the trap adapter still has preemption disabled. The irqchip
/// may subsequently classify the claim as a normal IRQ, NMI, or spurious. They
/// are not the hardirq-count boundary used by bottom-half context checks; that
/// boundary is tracked by `kirq::context`.
#[derive(Clone, Copy, Default)]
pub struct IrqLifecycleHooks {
    /// Called after the IRQ trap adapter disables preemption and before
    /// controller dispatch begins.
    pub on_irq_enter: Option<IrqLifecycleHook>,

    /// Called after the controller dispatch path completes and before
    /// preemption is re-enabled.
    pub on_irq_exit: Option<IrqLifecycleHook>,
}

static IRQ_LIFECYCLE_HOOKS: SpinNoIrq<IrqLifecycleHooks> = SpinNoIrq::new(IrqLifecycleHooks {
    on_irq_enter: None,
    on_irq_exit: None,
});
static IRQ_LIFECYCLE_HOOKS_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Register IRQ lifecycle hooks.
///
/// Only one owner may install hooks at a time. This keeps the first extension
/// point simple and explicit until a real notifier-chain or per-subsystem hook
/// model is needed.
pub fn register_irq_lifecycle_hooks(hooks: IrqLifecycleHooks) -> bool {
    let mut current = IRQ_LIFECYCLE_HOOKS.lock();
    if current.on_irq_enter.is_some() || current.on_irq_exit.is_some() {
        return false;
    }
    *current = hooks;
    IRQ_LIFECYCLE_HOOKS_ACTIVE.store(
        hooks.on_irq_enter.is_some() || hooks.on_irq_exit.is_some(),
        Ordering::Release,
    );
    true
}

/// Clear IRQ lifecycle hooks.
pub fn clear_irq_lifecycle_hooks() {
    *IRQ_LIFECYCLE_HOOKS.lock() = IrqLifecycleHooks::default();
    IRQ_LIFECYCLE_HOOKS_ACTIVE.store(false, Ordering::Release);
}

pub(crate) struct IrqLifecycleGuard {
    on_irq_exit: Option<IrqLifecycleHook>,
}

impl IrqLifecycleGuard {
    pub(crate) fn enter() -> Self {
        if !IRQ_LIFECYCLE_HOOKS_ACTIVE.load(Ordering::Acquire) {
            return Self { on_irq_exit: None };
        }
        let hooks = *IRQ_LIFECYCLE_HOOKS.lock();
        if let Some(hook) = hooks.on_irq_enter {
            hook();
        }
        Self {
            on_irq_exit: hooks.on_irq_exit,
        }
    }
}

impl Drop for IrqLifecycleGuard {
    fn drop(&mut self) {
        if let Some(hook) = self.on_irq_exit {
            hook();
        }
    }
}
