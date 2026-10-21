// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! GP task and bounded softirq execution. Ordinary callback admission is closed.

use core::{
    sync::atomic::{AtomicBool, Ordering},
    task::Waker,
};

use kerrno::{KError, KResult};
use kirq::softirq::SoftirqVec;
use klazy::Once;
use krcu_core::{CallbackDriver, CallbackState};
use ktask::{Expiration, TaskInner, Timer, TimerControl, future::PreparedTaskWait};
use ktime_types::{MonotonicInstant, TimeSpan};

use crate::context::READER_NODES;

const PROGRESS_INTERVAL_NS: u64 = 10_000_000;
// Bound one Rcu action's work, not queue capacity. The common softirq runner
// supplies the outer restart/time limits and ksoftirqd fallback.
const CALLBACK_BUDGET: usize = 32;
static SERVICE: Once<GracePeriodTask> = Once::new();

unittest::path_mod_test!(callback, "../ut-tests/callback.rs");

// Only controlled consumers can obtain the current-task reader until public
// read-side admission and the complete context protocol are available.
unittest::path_mod_test!(synchronize, "../ut-tests/synchronize.rs");

// The owning consumer remains confined to integration tests until the complete
// idle/user/IRQ/NMI context protocol has been validated.
unittest::path_mod_test!(owned, "../ut-tests/owned.rs");

struct GracePeriodTask {
    callbacks: CallbackState<'static, { kbuild_config::NR_CPUS }>,
    has_started: kspin::SpinNoPreempt<bool>,
    waker: Once<Waker>,
    has_demand: AtomicBool,
}

pub(super) fn callbacks() -> Option<&'static CallbackState<'static, { kbuild_config::NR_CPUS }>> {
    SERVICE.get().map(|service| &service.callbacks)
}

/// Prepares the GP task and waits sleepably for its parking/timer resources.
///
/// This is carrier setup, not general RCU readiness or callback admission.
/// The participating CPU set is fixed from discovery, not configured capacity.
///
/// # Errors
/// Returns `InvalidInput` outside sleepable task context, or `BadState` when
/// control has already been claimed. Returns `ResourceBusy` if the Rcu action
/// is occupied; registration failure leaves driver control unclaimed.
/// Allocation failure follows kernel policy.
///
/// # Safety
/// Call only after every discovered CPU's scheduler, timer IRQ and notification
/// services are initialized and its permanent CPU area is published.
/// No other coordinator may control READER_NODES.
pub unsafe fn init_service() -> KResult<()> {
    let mut startup = PreparedTaskWait::prepare()?;
    let service = SERVICE.call_once(|| {
        let participants = core::array::from_fn(|cpu| cpu < kcpu_id_map::nr_cpus());
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: initialized CPU discovery includes \
                      the running boot CPU"
        )]
        // SAFETY: the startup caller excludes another owner of these static
        // nodes. Construction preserves already-enrolled task readers.
        let callbacks = unsafe { CallbackState::new(&READER_NODES, participants) }
            .expect("RCU startup requires a discovered boot CPU");
        GracePeriodTask {
            callbacks,
            has_started: kspin::SpinNoPreempt::new(false),
            waker: Once::new(),
            has_demand: AtomicBool::new(false),
        }
    });
    let driver = service.claim_driver_with_action(|| {
        kirq::softirq::open_softirq(SoftirqVec::Rcu, process_callbacks)
    })?;
    let startup_waker = startup.waker();
    let task = ktask::prepare_task(TaskInner::new_pidless_kthread(
        move || service.run(driver, startup_waker),
        "rcu_gp".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    ktask::activate_task(&task);
    while service.waker.get().is_none() {
        startup.park();
    }
    service.request_pass();
    Ok(())
}

impl GracePeriodTask {
    /// Transfers a withdrawn node and requests progress without allocating.
    ///
    /// Rejection leaves the node untouched and caller-owned. NMI and unwatched
    /// entry are rejected before queue access; readiness and CPU participation
    /// are checked before transfer. This private admission boundary remains
    /// test-only until the owning callback API is qualified.
    ///
    /// # Safety
    /// On an admissible call, supply the unique, stable node and allocation
    /// provenance required by `CallbackState::submit`. Its callback and all
    /// cleanup must be nonblocking, non-unwinding and valid in softirq context.
    /// Withdraw all publication routes before submission and retain this service
    /// and callback resources through callback return. Hold no scheduler or RCU
    /// bookkeeping lock:
    /// notification without IPIs may wake the softirq daemon. On success do not
    /// access the transferred node without independent ownership.
    #[cfg(unittest)]
    unsafe fn submit_callback(&self, node: *mut krcu_core::CallbackNode) -> KResult<()> {
        if kirq::context::is_in_nmi() {
            return Err(KError::InvalidInput);
        }
        {
            let _guard = kspin::NoPreemptIrqSave::new();
            if !krcu_core::RcuContextIf::current_cpu_watching().is_watching() {
                return Err(KError::InvalidInput);
            }
            if self.waker.get().is_none() {
                return Err(KError::BadState);
            }
            let cpu = khal::percpu::this_cpu_id().as_usize();
            // SAFETY: the caller retains unique stable storage and supplies a
            // withdrawn object with softirq-safe cleanup. The guard keeps CPU
            // selection and queue transfer in one watched, non-NMI interval.
            if !unsafe { self.callbacks.submit(cpu, node) } {
                return Err(KError::BadState);
            }
        }
        // Ownership is already transferred; never touch node here. Notification
        // follows retained queue state and runs outside its lock and our guard.
        self.request_pass();
        Ok(())
    }

    /// Returns whether a reschedule notification was successfully requested,
    /// not whether the destination handled it or completed reader cleanup.
    #[cfg(feature = "ipi")]
    fn request_cpu_progress(&self, cpu: usize) -> bool {
        let cpu_id = kcpu_id_map::LogicalCpuId::new(cpu);
        if !self.callbacks.needs_cpu_observation(cpu)
            && !crate::context::needs_reader_cleanup(cpu_id)
        {
            return false;
        }
        // No bookkeeping lock is held. Do not consume cleanup demand remotely:
        // IPI receipt cannot certify that its task reached a safe exit point.
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: fixed participants completed \
                      notification startup before the GP task was activated"
        )]
        kipi::notify_reschedule(cpu_id).expect("RCU progress CPU completed boot services");
        true
    }

    /// Serializes action installation and the irreversible control claim.
    /// The installer must not block or reenter startup; failure must leave the
    /// action unchanged. Task creation and startup acknowledgment stay outside.
    fn claim_driver_with_action(
        &self,
        install_action_fn: impl FnOnce() -> bool,
    ) -> KResult<CallbackDriver<'_, 'static, { kbuild_config::NR_CPUS }>> {
        let mut has_started = self.has_started.lock();
        if *has_started {
            return Err(KError::BadState);
        }
        if !install_action_fn() {
            return Err(KError::ResourceBusy);
        }
        // Only this serialized startup path claims production control. Once
        // the action is installed, losing the driver is a fatal invariant bug.
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: has_started serializes the sole \
                      driver claim and rejects repeat startup"
        )]
        let driver = self
            .callbacks
            .claim_driver()
            .expect("RCU startup exclusively owns the driver claim");
        *has_started = true;
        Ok(driver)
    }

    /// Publishes demand before requesting deferred notification. With IPIs this
    /// cannot wake the GP task inline; without IPIs callers must hold no scheduler
    /// or RCU lock because raising softirq may wake its daemon. Never NMI-safe.
    fn request_pass(&self) {
        self.has_demand.store(true, Ordering::Release);
        #[cfg(feature = "ipi")]
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: service callers run on discovered \
                      CPUs after notification startup"
        )]
        kipi::notify_rcu(khal::percpu::this_cpu_id())
            .expect("RCU notifier runs on an initialized present CPU");
        #[cfg(not(feature = "ipi"))]
        kirq::softirq::raise_softirq(SoftirqVec::Rcu);
    }

    fn run(
        &'static self,
        mut driver: CallbackDriver<'static, 'static, { kbuild_config::NR_CPUS }>,
        startup_waker: Waker,
    ) -> ! {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: the dedicated GP task enters with no \
                      exclusion guards or reader scope"
        )]
        let mut wait =
            PreparedTaskWait::prepare().expect("RCU GP task starts in sleepable task context");
        let timer = Timer::new(move |_: TimerControl<'_>, _: Expiration| self.request_pass());
        // Publishing the waker acknowledges both prepared resources. The wait
        // itself remains bound to this task, never to the boot task.
        self.waker.call_once(|| wait.waker());
        startup_waker.wake();
        let mut deadline = None::<u64>;
        loop {
            // Consume before processing: demand arriving during the pass must
            // survive for the next iteration. The prepared waker retains races
            // between this exchange, processing and actual park.
            let has_demand = self.has_demand.swap(false, Ordering::AcqRel);
            let now = khal::time::monotonic_time_nanos();
            let is_due = deadline.is_some_and(|deadline| now >= deadline);
            if has_demand || is_due {
                for cpu in 0..kcpu_id_map::nr_cpus() {
                    #[expect(
                        clippy::expect_used,
                        reason = "CG-021-no-panic-on-reachable-failure: fixed GP participants \
                                  published their permanent CPU areas before service activation"
                    )]
                    let area = kcpu_slot::area(kcpu_id_map::LogicalCpuId::new(cpu))
                        .expect("RCU participant CPU area is published");
                    // SAFETY: permanent storage matches this discovered CPU.
                    // Idle/user windows and supported IRQ/NMI transitions are
                    // paired before protected access. Controlled interrupt
                    // scopes finish before handler return without enrollment.
                    // This GP task is watched and holds no RCU lock.
                    let reported = unsafe {
                        self.callbacks
                            .observe_watching(cpu, crate::context::cpu_watching(&area))
                    };
                    if !reported {
                        // SAFETY: fresh watched inspection with no RCU lock.
                        // A prior CPU observation may still have awaited an old
                        // reader; sampling alone does not report its later exit.
                        unsafe { self.callbacks.report_ready(cpu) };
                    }
                }
                let is_active = driver.advance();
                // Progress kicks and callback notifications run without any
                // bookkeeping lock. Eligibility is persistent before its wake.
                for cpu in 0..kcpu_id_map::nr_cpus() {
                    #[cfg(feature = "ipi")]
                    if is_active && (deadline.is_none() || is_due) {
                        // A kick only requests scheduling. The scheduler hook
                        // must still preserve the outgoing reader and report;
                        // neither IPI acceptance nor expiry is quiescence.
                        self.request_cpu_progress(cpu);
                    }
                    if self.callbacks.has_ready(cpu) {
                        #[cfg(feature = "ipi")]
                        #[expect(
                            clippy::expect_used,
                            reason = "CG-021-no-panic-on-reachable-failure: every discovered \
                                      callback CPU completed notification startup before service \
                                      activation"
                        )]
                        kipi::notify_rcu(kcpu_id_map::LogicalCpuId::new(cpu))
                            .expect("RCU callback CPU completed boot services");
                        #[cfg(not(feature = "ipi"))]
                        kirq::softirq::raise_softirq(SoftirqVec::Rcu);
                    }
                }
                if !is_active {
                    deadline = None;
                    timer.cancel_sync();
                } else if let Some(next) =
                    progress_rearm_deadline(now, deadline, khal::time::monotonic_time_nanos())
                {
                    #[expect(
                        clippy::expect_used,
                        reason = "CG-021-no-panic-on-reachable-failure: startup validates timer \
                                  owners and the deadline uses saturating monotonic nanoseconds"
                    )]
                    // SAFETY: startup waited for every possible owner CPU's
                    // timer service. The handler only records demand and wakes,
                    // never waits, reenters timer dispatch or holds RCU locks.
                    unsafe {
                        timer.start(MonotonicInstant::from_span_since_origin(
                            TimeSpan::from_nanos(next),
                        ))
                    }
                    .expect("RCU progress deadline and timer owner are validated");
                    deadline = Some(next);
                }
            }
            wait.park();
        }
    }
}

fn process_callbacks() {
    let Some(service) = SERVICE.get() else {
        return;
    };
    if service.has_demand.load(Ordering::Acquire)
        && let Some(waker) = service.waker.get()
    {
        // Leave demand for the GP task to consume. A new notification arriving
        // during this action remains pending in the ordinary softirq machinery.
        waker.wake_by_ref();
    }
    let cpu = khal::percpu::this_cpu_id().as_usize();
    // SAFETY: the softirq carrier excludes migration and holds no RCU lock.
    // Admission is limited to controlled test objects and synchronous-wait
    // markers, all retained through their nonblocking callbacks. Periods use
    // real scheduler/watching observations; general admission remains closed.
    unsafe { service.callbacks.invoke_ready(cpu, CALLBACK_BUDGET) };
    if service.callbacks.has_ready(cpu) {
        // Wait only for eligible work, never spin on a GP blocked by readers.
        // Existing softirq restart limits and ksoftirqd provide further service.
        kirq::softirq::raise_softirq(SoftirqVec::Rcu);
    }
}

/// Retains early-wakeup timing; a performed progress round gets a fresh interval.
fn progress_rearm_deadline(observed_ns: u64, deadline: Option<u64>, rearm_ns: u64) -> Option<u64> {
    if deadline.is_some_and(|deadline| observed_ns < deadline) {
        return None;
    }
    // Like Linux's post-FQS wait, measure the next interval after the work,
    // not from the earlier sample used to decide whether to check progress.
    // Saturation preserves representability, not delivery at the clock horizon.
    Some(rearm_ns.saturating_add(PROGRESS_INTERVAL_NS))
}

unittest::path_mod_test!(tests, "../ut-tests/rcu_service.rs");
