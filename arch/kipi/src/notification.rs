// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Allocation-free CPU notifications and their one-way boot handshake.

use core::sync::atomic::{AtomicUsize, Ordering};

use kcpu_id_map::{LogicalCpuId, raw_cpu_id};
use khal::percpu::this_cpu_id;

use crate::{KipiError, Result};

const ENABLED: usize = 1;
const RESCHEDULE: usize = 2;

struct PendingWord(AtomicUsize);

impl PendingWord {
    const fn new() -> Self {
        Self(AtomicUsize::new(0))
    }

    fn request(&self) -> bool {
        // Publication and observing readiness must share one RMW: either
        // enable's drain sees this request, or this sender must issue an IPI.
        self.0.fetch_or(RESCHEDULE, Ordering::AcqRel) & ENABLED != 0
    }

    fn enable(&self) {
        self.0.fetch_or(ENABLED, Ordering::AcqRel);
    }

    fn take(&self) -> bool {
        // An unrelated early IPI must not consume requests before boot drain.
        if self.0.load(Ordering::Acquire) & ENABLED == 0 {
            return false;
        }
        self.0.fetch_and(ENABLED, Ordering::AcqRel) & RESCHEDULE != 0
    }
}

static PENDING: [PendingWord; kbuild_config::NR_CPUS] =
    [const { PendingWord::new() }; kbuild_config::NR_CPUS];

/// Statically linked scheduler endpoint supplied by the task subsystem.
#[kiface::interface]
pub trait RescheduleIpiIf {
    /// Marks a local reschedule request, or does nothing without preemption.
    ///
    /// Called with local IRQs disabled after scheduler initialization, either
    /// during boot drain or from the IPI handler. Must not allocate or sleep.
    fn on_reschedule();
}

/// Requests a scheduler CPU kick without allocating or queueing a callback.
///
/// The caller must first publish its task state to an initialized destination
/// run queue. Requests before destination enablement are retained until boot
/// drain. Successful return means accepted, not executed. Repeated requests
/// may coalesce; after enablement every request sends an IPI, including self.
/// May be called with IRQs disabled and scheduler locks held; does not sleep.
///
/// # Errors
/// Returns [`KipiError::InvalidCpuId`] for an out-of-range or absent CPU.
pub fn notify_reschedule(cpu_id: LogicalCpuId) -> Result<()> {
    let pending = PENDING
        .get(cpu_id.as_usize())
        .filter(|_| raw_cpu_id(cpu_id).is_some())
        .ok_or(KipiError::InvalidCpuId)?;
    if pending.request() {
        kirq::notify_cpu(
            kbuild_config::IPI_IRQ,
            kirq::TargetCpu::Specific(cpu_id.as_usize()),
        );
    }
    Ok(())
}

/// Enables fixed notifications and drains requests retained during boot.
///
/// Call once from each CPU's CPU-bound startup path after its scheduler, IPI
/// queue, controller and shared IPI handler are initialized. The caller must
/// remain CPU-bound even across preemption checks inside IRQ line enablement.
/// Owns local IPI line enablement and masks local IRQs through publication and
/// drain. There is no shutdown or CPU-hotplug protocol, or queued closure dispatch.
/// Boot callers must defer preemption until their remaining per-CPU services
/// are prepared: IRQ masking does not prevent a later lock-drop reschedule.
///
/// # Panics
/// Panics if the IPI line cannot be enabled; readiness is not published on failure.
pub fn enable_fixed_notifications() {
    // Do not run a preemption check when leaving boot drain: AP initialization
    // still has work to finish before entering its normal scheduling loop.
    let _guard = kspin::IrqSave::new();
    kirq::try_enable(kbuild_config::IPI_IRQ, true).expect("fixed IPI line enable failed");
    PENDING[this_cpu_id().as_usize()].enable();
    handle_pending();
}

pub(crate) fn handle_pending() {
    if PENDING[this_cpu_id().as_usize()].take() {
        RescheduleIpiIf::on_reschedule();
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, def_test};

    use super::PendingWord;

    #[def_test]
    fn fixed_early_request_survives_unrelated_ipi() {
        let pending = PendingWord::new();
        assert!(!pending.request());
        assert!(!pending.take());
        pending.enable();
        assert!(pending.take());
        assert!(!pending.take());
    }

    #[def_test]
    fn fixed_request_between_enable_and_drain() {
        let pending = PendingWord::new();
        pending.enable();
        assert!(pending.request());
        assert!(pending.take());
        assert!(!pending.take());
    }

    #[def_test]
    fn fixed_requests_coalesce_without_losing_later_request() {
        let pending = PendingWord::new();
        pending.enable();
        assert!(!pending.take());
        assert!(pending.request());
        assert!(pending.request());
        assert!(pending.take());
        assert!(pending.request());
        assert!(pending.take());
        assert!(!pending.take());
    }
}
