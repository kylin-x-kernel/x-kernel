// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform performance monitoring interface.

use kplat_macros::device_interface;

#[device_interface]
pub trait PmuInterruptIf {
    /// Register the PMU overflow-dispatch handler on the platform's PMU
    /// interrupt line.
    ///
    /// The platform owns the architecture-specific wiring: the PMU IRQ
    /// number, the interrupt descriptor, and the delivery mode (a normal IRQ
    /// handler, or an NMI handler when the PMU is the compiled NMI source).
    /// Called once at boot, before IRQs are enabled.
    fn register_overflow_irq() -> bool;
    /// Stops all participating CPUs and retires the shared PMU NMI handler.
    ///
    /// Requires sleepable task context after boot CPU services are ready; CPU
    /// hotplug and system stop must be excluded. The periodic-source consumer
    /// must authorize losing service. Only exclusive periodic PMU ownership is
    /// supported, not concurrent perf event counters. The provider serializes
    /// shutdown/restoration and rejects new local starts while closing.
    ///
    /// # Errors
    /// Unsupported providers reject the operation. Transport or local teardown
    /// failure retains the original registration and remaining CPU obligations
    /// for a later call to retry. Success includes RCU retirement; it does not
    /// restart counters. Repeated calls after success are harmless.
    fn shutdown_overflow_irq() -> kerrno::KResult<()>;

    /// Republishes a PMU handler after successful global shutdown.
    ///
    /// Requires sleepable task context and the same CPU/service exclusions as
    /// shutdown. Local preparation, IRQ enable and counter arming remain explicit
    /// per-CPU operations; no old callback is restarted automatically.
    ///
    /// # Errors
    /// Rejects incomplete shutdown, unsupported providers or failed registration.
    /// Failure keeps local source admission closed so restoration can be retried.
    fn restore_overflow_irq() -> kerrno::KResult<()>;
    /// Enable the PMU interrupt line on the current CPU.
    fn enable_irq();
}
