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
use krcu::{CallbackDriver, CallbackState};
use ktime_types::{MonotonicInstant, TimeSpan};

use super::READER_NODES;
use crate::{Expiration, TaskInner, Timer, TimerControl, future::PreparedTaskWait};

const PROGRESS_INTERVAL_NS: u64 = 10_000_000;
// Bound one Rcu action's work, not queue capacity. The common softirq runner
// supplies the outer restart/time limits and ksoftirqd fallback.
const CALLBACK_BUDGET: usize = 32;
static SERVICE: Once<GracePeriodTask> = Once::new();

// Only controlled consumers can obtain the current-task reader until public
// read-side admission and the complete context protocol are available.
#[cfg(unittest)]
#[path = "synchronize.rs"]
mod synchronize;

// The owning consumer remains confined to integration tests until the complete
// idle/user/IRQ/NMI context protocol has been validated.
#[cfg(unittest)]
#[path = "owned.rs"]
mod owned;

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
/// services are initialized. No other coordinator may control READER_NODES.
pub unsafe fn init_service() -> KResult<()> {
    let mut startup = PreparedTaskWait::prepare()?;
    let service = SERVICE.call_once(|| {
        let participants = core::array::from_fn(|cpu| cpu < kcpu_id_map::nr_cpus());
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
    let task = crate::prepare_task(TaskInner::new_pidless_kthread(
        move || service.run(driver, startup_waker),
        "rcu_gp".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    crate::activate_task(&task);
    while service.waker.get().is_none() {
        startup.park();
    }
    service.request_pass();
    Ok(())
}

impl GracePeriodTask {
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
                    // SAFETY: this GP task stays in watched kernel context,
                    // holds no RCU lock and inspects each node afresh. This only
                    // reports recorded observations after old readers exit.
                    unsafe { self.callbacks.report_ready(cpu) };
                }
                let is_active = driver.advance();
                // Progress kicks and callback notifications run without any
                // bookkeeping lock. Eligibility is persistent before its wake.
                for cpu in 0..kcpu_id_map::nr_cpus() {
                    #[cfg(feature = "ipi")]
                    if is_active
                        && (deadline.is_none() || is_due)
                        && self.callbacks.needs_cpu_observation(cpu)
                    {
                        // A kick only requests scheduling. The scheduler hook
                        // must still preserve the outgoing reader and report;
                        // neither IPI acceptance nor expiry is quiescence.
                        kipi::notify_reschedule(kcpu_id_map::LogicalCpuId::new(cpu))
                            .expect("RCU observation CPU completed boot services");
                    }
                    if self.callbacks.has_ready(cpu) {
                        #[cfg(feature = "ipi")]
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
    // real scheduler observations; general admission awaits context integration.
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

unittest::path_mod_test!(tests, "../../ut-tests/rcu_service.rs");
