// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Runtime-owned interrupt readiness, separate from boot rendezvous counts.
//!
//! Only the primary/AP startup paths publish their own CPU after local sources
//! and IRQ-exit services are prepared. Controller-specific routing capability
//! remains with the irqchip. This module does not implement CPU hot-unplug.

use core::sync::atomic::{AtomicBool, Ordering};

use kcpu_id_map::{KCpuMask, KCpuMaskExt, LogicalCpuId};

/// Independent, monotonic boot publications indexed by logical CPU identity.
struct InterruptReadiness {
    is_ready: [AtomicBool; kbuild_config::NR_CPUS],
}

impl InterruptReadiness {
    const fn new() -> Self {
        Self {
            is_ready: [const { AtomicBool::new(false) }; kbuild_config::NR_CPUS],
        }
    }

    fn publish(&self, cpu: LogicalCpuId) {
        // Release all of this CPU's preparation to readers selecting a target.
        self.is_ready[cpu.as_usize()].store(true, Ordering::Release);
    }

    fn snapshot(&self) -> KCpuMask {
        let mut mask = KCpuMask::new();
        for (cpu, is_ready) in self.is_ready.iter().enumerate() {
            // Each observed bit acquires its own CPU's preparation. A missed
            // concurrent publication only makes this sample conservative.
            if is_ready.load(Ordering::Acquire) {
                mask.set_logical(LogicalCpuId::new(cpu), true);
            }
        }
        mask
    }
}

static INTERRUPT_READINESS: InterruptReadiness = InterruptReadiness::new();

/// Publishes the current boot CPU after local IRQ setup, before enabling IRQs.
/// Both callers remain on their startup CPU until they enter normal execution.
pub(super) fn publish_current_cpu() {
    INTERRUPT_READINESS.publish(khal::percpu::this_cpu_id());
    #[cfg(unittest)]
    super::tests_irq_startup::record_interrupt_ready();
}

#[kiface::provide]
impl kiface_cpu_api::CpuInterruptReadinessIf {
    fn ready_cpus() -> KCpuMask {
        INTERRUPT_READINESS.snapshot()
    }
}

unittest::path_mod_test!(tests, "../ut-tests/cpu_readiness.rs");
