// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `kcpu_slot` execution-context contracts for the guards in this crate.
//!
//! Guards that already prevent migration implement [`CpuPin`].
//! [`NoPreemptIrqSave`] additionally keeps local IRQ handlers from running
//! for its whole lifetime, so it is also a [`CpuLocalGuard`] — the only one:
//! `NoPreempt` leaves local IRQs enabled, and `IrqSave` leaves preemption
//! control to the caller, so neither claims the stronger capability.
//!
//! On host test builds the kernel guard names are all aliases of [`NoOp`];
//! that single type receives one combined impl set so generic test code can
//! treat any guard as a pin.

#[cfg(target_os = "none")]
mod imp {
    use kcpu_slot::{CpuId, CpuLocalGuard, CpuPin};
    use kiface_cpu_api::CurrentCpuIf;

    use crate::guard::types::{IrqSave, NoPreempt, NoPreemptIrqSave};

    // SAFETY: Saving and disabling local IRQs blocks the tick or IPI a
    // migration would need, so the context stays on the CPU reported by
    // `CurrentCpuIf` for the guard's whole lifetime.
    unsafe impl CpuPin for IrqSave {
        fn current_cpu(&self) -> CpuId {
            CurrentCpuIf::current_cpu()
        }
    }

    // SAFETY: With the `preempt` feature the guard disables preemption, so the
    // context cannot migrate. Without it the guard is a no-op, but the kernel then
    // runs only a cooperative (non-preemptive) scheduler, so a non-blocking scope
    // cannot migrate either. Either way the context stays on the CPU reported by
    // `CurrentCpuIf` for the guard's whole lifetime.
    unsafe impl CpuPin for NoPreempt {
        fn current_cpu(&self) -> CpuId {
            CurrentCpuIf::current_cpu()
        }
    }

    // SAFETY: Local IRQs are disabled (and preemption too when the `preempt`
    // feature is on), so no other context can run on this CPU while the guard
    // is alive. This also excludes the same-slot local IRQ reentry that
    // [`CpuLocalGuard`] rules out.
    unsafe impl CpuPin for NoPreemptIrqSave {
        fn current_cpu(&self) -> CpuId {
            CurrentCpuIf::current_cpu()
        }
    }

    // SAFETY: See the `CpuPin` impl above.
    unsafe impl CpuLocalGuard for NoPreemptIrqSave {}
}

#[cfg(not(target_os = "none"))]
mod imp {
    use kcpu_slot::{CpuId, CpuLocalGuard, CpuPin};
    use kiface_cpu_api::CurrentCpuIf;

    use crate::guard::types::NoOp;

    // SAFETY: Host test builds alias every kernel guard to `NoOp`; tests are
    // single-threaded and never migrate, so the reported CPU is stable.
    unsafe impl CpuPin for NoOp {
        fn current_cpu(&self) -> CpuId {
            CurrentCpuIf::current_cpu()
        }
    }

    // SAFETY: See the `CpuPin` impl above.
    unsafe impl CpuLocalGuard for NoOp {}
}
