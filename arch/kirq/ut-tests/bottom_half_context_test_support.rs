// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kspin::NoPreemptIrqSave;

use super::HardIrqContextGuard;

/// Exercises the real software NMI context/watching pair without hardware.
///
/// # Safety
/// The caller must initialize CPU slots and current, exclude ordinary IRQs
/// and migration, and retain that exclusion through return. The callback
/// must return normally on this stack, without scheduling or leaving nested
/// entries or protected work unfinished. This helper does not acquire a pin.
pub unsafe fn with_nmi_context<R>(operation_fn: impl FnOnce() -> R) -> R {
    // SAFETY: the fixture supplies initialized, pinned CPU/current state
    // and balanced nonscheduling execution, matching the real entry helper.
    unsafe { crate::runtime::rcu::with_nmi_context(operation_fn) }
}

/// Test-only hardirq context with local IRQs masked and preemption disabled.
pub struct ScopedHardIrqContext {
    _guard: HardIrqContextGuard,
    // Restore context before permitting task preemption or local IRQs.
    _irq_pin: NoPreemptIrqSave,
}

impl ScopedHardIrqContext {
    /// Pins the CPU before entering synthetic hardirq context for tests.
    pub fn enter() -> Self {
        let irq_pin = NoPreemptIrqSave::new();
        Self {
            _guard: HardIrqContextGuard::enter(),
            _irq_pin: irq_pin,
        }
    }
}
