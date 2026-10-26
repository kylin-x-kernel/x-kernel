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
        const BOTTOM_HALF = 1 << 3;
        const HIGHPRI_BOTTOM_HALF = 1 << 4;
        const RCU = 1 << 5;
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

/// Requests the destination CPU's ordinary BH service without allocation.
///
/// Publish source demand and release its locks before calling. The receiver only
/// raises `kirq::softirq::SoftirqVec::Tasklet`; callbacks run later in softirq
/// context, never in this function or the fixed IPI handler. The caller must
/// prepare the destination service and install its softirq action before
/// publishing demand. This API does not register a service or retain its storage.
///
/// May run in task, softirq or hardirq context, including with IRQs disabled.
/// Does not sleep or enqueue a closure. Success means accepted, not completed;
/// repeated requests may coalesce. Before fixed notifications are enabled,
/// requests remain pending for boot drain; afterward every request sends an IPI,
/// including a request to self. Does not support CPU hotplug or NMI context.
///
/// # Errors
/// Returns [`KipiError::InvalidCpuId`] for an out-of-range or absent CPU.
pub fn notify_bottom_half(cpu_id: LogicalCpuId) -> Result<()> {
    notify(cpu_id, NotificationFlags::BOTTOM_HALF)
}

/// Requests the destination CPU's high-priority BH service without allocation.
///
/// Has the publication, context, readiness and coalescing contract of
/// [`notify_bottom_half`], but raises `kirq::softirq::SoftirqVec::High` instead
/// of `Tasklet`. The two categories retain independent pending bits.
///
/// # Errors
/// Returns [`KipiError::InvalidCpuId`] for an out-of-range or absent CPU.
pub fn notify_highpri_bottom_half(cpu_id: LogicalCpuId) -> Result<()> {
    notify(cpu_id, NotificationFlags::HIGHPRI_BOTTOM_HALF)
}

/// Requests deferred RCU processing on an initialized destination CPU.
///
/// Publish RCU demand first and install the Rcu softirq action before calling.
/// The receiver only marks that vector pending; it never directly wakes the GP
/// task or executes callbacks. Requests coalesce and survive the boot handshake.
/// After enablement every request sends an IPI, including requests to self.
/// Does not allocate or sleep. IRQ-off scheduler paths may use this transport;
/// release their locks before reenabling IRQs. NMI and CPU hotplug are unsupported.
///
/// # Errors
/// Returns [`KipiError::InvalidCpuId`] for an out-of-range or absent CPU.
pub fn notify_rcu(cpu_id: LogicalCpuId) -> Result<()> {
    notify(cpu_id, NotificationFlags::RCU)
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
/// kirq's softirq pending storage must be usable. BH actions
/// and destination services must be installed before their producers publish
/// demand. Boot drain only marks softirq pending; a daemon started afterward
/// observes that retained state before sleeping.
///
/// # Panics
/// Panics if the IPI line cannot be enabled; readiness is not published on failure.
pub fn enable_fixed_notifications() {
    // Do not run a preemption check when leaving boot drain: AP initialization
    // still has work to finish before entering its normal scheduling loop.
    let _guard = kspin::IrqSave::new();
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: the fixed IPI IRQ cannot be \
                  enabled; CPU notification initialization has no error return or recovery"
    )]
    kirq::try_enable(kbuild_config::IPI_IRQ, true).expect("fixed IPI line enable failed");
    PENDING[this_cpu_id().as_usize()].enable();
    handle_pending();
}

pub(crate) fn handle_pending() {
    let reasons = PENDING[this_cpu_id().as_usize()].take();
    #[cfg(feature = "timer-reprogram")]
    if reasons.contains(NotificationFlags::TIMER_REPROGRAM) {
        kiface_time_api::LocalTimerReprogramIf::on_timer_reprogram();
    }
    #[cfg(feature = "reschedule")]
    if reasons.contains(NotificationFlags::RESCHEDULE) {
        kiface_task_api::TaskRescheduleRequestIf::on_reschedule();
    }
    handle_bottom_half(reasons);
}

fn handle_bottom_half(reasons: NotificationFlags) {
    use kirq::softirq::{SoftirqVec, raise_softirq_irqoff};

    if reasons.contains(NotificationFlags::HIGHPRI_BOTTOM_HALF) {
        raise_softirq_irqoff(SoftirqVec::High);
    }
    if reasons.contains(NotificationFlags::BOTTOM_HALF) {
        raise_softirq_irqoff(SoftirqVec::Tasklet);
    }
    if reasons.contains(NotificationFlags::RCU) {
        raise_softirq_irqoff(SoftirqVec::Rcu);
        #[cfg(unittest)]
        rcu_unittest::record_delivery();
    }
}

unittest::path_mod_test!(bottom_half_unittest, "../ut-tests/bottom_half_unittest.rs");

unittest::path_mod_test!(rcu_unittest, "../ut-tests/rcu_unittest.rs");

unittest::path_mod_test!(
    notification_unittest,
    "../ut-tests/notification_unittest.rs"
);

#[cfg(feature = "reschedule")]
#[kiface::provide]
impl kiface_cpu_api::CpuRescheduleNotifyIf {
    fn notify_reschedule(cpu: LogicalCpuId) -> kerrno::KResult<()> {
        notify_reschedule(cpu).map_err(|_| kerrno::KError::BadState)
    }
}
