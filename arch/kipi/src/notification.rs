// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Allocation-free CPU notifications and their one-way boot handshake.

use core::sync::atomic::{AtomicUsize, Ordering};

use kcpu_id_map::{LogicalCpuId, raw_cpu_id};
use khal::percpu::this_cpu_id;

use crate::{KipiError, Result};

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct NotificationFlags: usize {
        const ENABLED = 1 << 0;
        #[cfg(any(feature = "reschedule", unittest))]
        const RESCHEDULE = 1 << 1;
        #[cfg(any(feature = "timer-reprogram", unittest))]
        const TIMER_REPROGRAM = 1 << 2;
    }
}

struct PendingWord(AtomicUsize);

impl PendingWord {
    const fn new() -> Self {
        Self(AtomicUsize::new(0))
    }

    fn request(&self, reason: NotificationFlags) -> bool {
        // Publication and observing readiness must share one RMW: either
        // enable's drain sees this request, or this sender must issue an IPI.
        NotificationFlags::from_bits_retain(self.0.fetch_or(reason.bits(), Ordering::AcqRel))
            .contains(NotificationFlags::ENABLED)
    }

    fn enable(&self) {
        self.0
            .fetch_or(NotificationFlags::ENABLED.bits(), Ordering::AcqRel);
    }

    fn take(&self) -> NotificationFlags {
        // An unrelated early IPI must not consume requests before boot drain.
        if !NotificationFlags::from_bits_retain(self.0.load(Ordering::Acquire))
            .contains(NotificationFlags::ENABLED)
        {
            return NotificationFlags::empty();
        }
        NotificationFlags::from_bits_retain(
            self.0
                .fetch_and(NotificationFlags::ENABLED.bits(), Ordering::AcqRel),
        )
        .difference(NotificationFlags::ENABLED)
    }
}

static PENDING: [PendingWord; kbuild_config::NR_CPUS] =
    [const { PendingWord::new() }; kbuild_config::NR_CPUS];

/// Statically linked scheduler endpoint supplied by the task subsystem.
#[cfg(feature = "reschedule")]
#[kiface::interface]
pub trait RescheduleIpiIf {
    /// Marks a local reschedule request, or does nothing without preemption.
    ///
    /// Called with local IRQs disabled after scheduler initialization, either
    /// during boot drain or from the IPI handler. Must not allocate or sleep.
    fn on_reschedule();
}

/// Statically linked local hardware-timer arbitration endpoint.
#[cfg(feature = "timer-reprogram")]
#[kiface::interface]
pub trait TimerReprogramIpiIf {
    /// Re-reads local deadline sources and refreshes the hardware timer.
    ///
    /// Runs with local IRQs disabled, after per-CPU timer state is initialized,
    /// during boot drain or IPI handling. Must not allocate, sleep, invoke timer
    /// callbacks or acquire a run-queue lock. The endpoint must synchronize
    /// deadline reads through the hardware write, not program a stale snapshot.
    fn on_timer_reprogram();
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
#[cfg(feature = "reschedule")]
pub fn notify_reschedule(cpu_id: LogicalCpuId) -> Result<()> {
    notify(cpu_id, NotificationFlags::RESCHEDULE)
}

/// Requests local timer arbitration on a destination CPU without allocation.
///
/// Publish the new deadline state before calling. Release timer locks first:
/// the receiver may need them to refresh its hardware timer. This does not
/// deliver timer callbacks or mark a reschedule request. Success means accepted,
/// not completed; repeated requests may coalesce. Before notification enablement
/// requests remain pending for boot drain; afterward every request sends an IPI,
/// including a request to self. Does not sleep or enqueue a closure.
///
/// # Errors
/// Returns [`KipiError::InvalidCpuId`] for an out-of-range or absent CPU.
#[cfg(feature = "timer-reprogram")]
pub fn notify_timer_reprogram(cpu_id: LogicalCpuId) -> Result<()> {
    notify(cpu_id, NotificationFlags::TIMER_REPROGRAM)
}

fn notify(cpu_id: LogicalCpuId, reason: NotificationFlags) -> Result<()> {
    let pending = PENDING
        .get(cpu_id.as_usize())
        .filter(|_| raw_cpu_id(cpu_id).is_some())
        .ok_or(KipiError::InvalidCpuId)?;
    if pending.request(reason) {
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
/// queue, timer state, controller and shared IPI handler are initialized. The
/// caller must remain CPU-bound even across preemption checks inside IRQ line enablement.
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
    let reasons = PENDING[this_cpu_id().as_usize()].take();
    #[cfg(feature = "timer-reprogram")]
    if reasons.contains(NotificationFlags::TIMER_REPROGRAM) {
        TimerReprogramIpiIf::on_timer_reprogram();
    }
    #[cfg(feature = "reschedule")]
    if reasons.contains(NotificationFlags::RESCHEDULE) {
        RescheduleIpiIf::on_reschedule();
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, assert_eq, def_test};

    use super::{NotificationFlags, PendingWord};

    #[def_test]
    fn fixed_early_request_survives_unrelated_ipi() {
        let pending = PendingWord::new();
        assert!(!pending.request(NotificationFlags::RESCHEDULE));
        assert!(pending.take().is_empty());
        pending.enable();
        assert_eq!(pending.take(), NotificationFlags::RESCHEDULE);
        assert!(pending.take().is_empty());
    }

    #[def_test]
    fn fixed_request_between_enable_and_drain() {
        let pending = PendingWord::new();
        pending.enable();
        assert!(pending.request(NotificationFlags::RESCHEDULE));
        assert_eq!(pending.take(), NotificationFlags::RESCHEDULE);
        assert!(pending.take().is_empty());
    }

    #[def_test]
    fn fixed_requests_coalesce_without_losing_later_request() {
        let pending = PendingWord::new();
        pending.enable();
        assert!(pending.take().is_empty());
        assert!(pending.request(NotificationFlags::RESCHEDULE));
        assert!(pending.request(NotificationFlags::RESCHEDULE));
        assert_eq!(pending.take(), NotificationFlags::RESCHEDULE);
        assert!(pending.request(NotificationFlags::RESCHEDULE));
        assert_eq!(pending.take(), NotificationFlags::RESCHEDULE);
        assert!(pending.take().is_empty());
    }

    #[def_test]
    fn timer_and_reschedule_survive_boot_drain_together() {
        let pending = PendingWord::new();
        assert!(!pending.request(NotificationFlags::TIMER_REPROGRAM));
        assert!(!pending.request(NotificationFlags::RESCHEDULE));
        assert!(pending.take().is_empty());
        pending.enable();
        assert_eq!(
            pending.take(),
            NotificationFlags::TIMER_REPROGRAM | NotificationFlags::RESCHEDULE
        );
        assert!(pending.take().is_empty());
    }

    #[def_test]
    fn timer_requests_coalesce_and_preserve_requests_during_service() {
        let pending = PendingWord::new();
        pending.enable();
        assert!(pending.request(NotificationFlags::TIMER_REPROGRAM));
        assert!(pending.request(NotificationFlags::TIMER_REPROGRAM));
        assert_eq!(pending.take(), NotificationFlags::TIMER_REPROGRAM);
        // A sender after the drain must leave work for the next IPI, including
        // when the receiver is still programming hardware for the old request.
        assert!(pending.request(NotificationFlags::TIMER_REPROGRAM));
        assert!(pending.request(NotificationFlags::RESCHEDULE));
        assert_eq!(
            pending.take(),
            NotificationFlags::TIMER_REPROGRAM | NotificationFlags::RESCHEDULE
        );
        assert!(pending.take().is_empty());
    }

    #[cfg(feature = "timer-reprogram")]
    #[def_test]
    fn timer_notification_rejects_out_of_range_cpu() {
        assert_eq!(
            super::notify_timer_reprogram(kbuild_config::NR_CPUS.into()),
            Err(crate::KipiError::InvalidCpuId)
        );
    }
}
