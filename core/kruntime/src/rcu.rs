// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Concrete task storage and notification adapters for RCU.

use core::task::Waker;

use kcpu_id_map::LogicalCpuId;
use kerrno::KResult;
use klazy::Lazy;
use krcu::host::{
    CurrentReader, PreparedGpTask, ProgressTimer, RcuExecutionIf, RcuNotifyIf, RcuSoftirqIf,
    RcuTaskIf, RcuTimerIf, TaskWait,
};
use ksync::Mutex;
use ktask::{Expiration, Timer, TimerControl, TimerHandler, future::PreparedTaskWait};
use ktime_types::{MonotonicInstant, TimeSpan};

// One permanent boot-owned RCU service uses this gate. Its markers stay in krcu
// and are never accessed by this adapter or by the mutex's protected payload.
static RCU_DRAIN_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

#[kiface::provide]
impl RcuExecutionIf {
    fn current_cpu() -> LogicalCpuId {
        khal::percpu::this_cpu_id()
    }

    fn is_in_exception_context() -> bool {
        khal::context::in_exception_context()
    }

    fn with_exception_snapshot(body_fn: &mut dyn FnMut(bool)) {
        let _irq = kspin::IrqSave::new();
        // SAFETY: the guard masks ordinary IRQs across slot selection and load;
        // it also keeps the body on this CPU without a preemption-count update.
        let is_active = unsafe { khal::context::in_exception_context_irqoff() };
        body_fn(is_active);
    }

    fn is_in_nmi() -> bool {
        kirq::context::is_in_nmi()
    }

    fn is_in_interrupt_context() -> bool {
        kirq::context::is_in_interrupt_context()
    }

    fn is_in_interrupt_handler() -> bool {
        // The common query checks NMI before taking an ordinary context snapshot.
        matches!(
            kirq::context::interrupt_context_level(),
            kirq::context::InterruptContextLevel::Nmi
                | kirq::context::InterruptContextLevel::Hardirq
                | kirq::context::InterruptContextLevel::Softirq
        )
    }
}

struct ProgressAlarm<H: TimerHandler + 'static> {
    timer: Timer<H>,
}

impl<H: TimerHandler + 'static> ProgressTimer for ProgressAlarm<H> {
    unsafe fn arm(&mut self, deadline_ns: u64) -> KResult<()> {
        // SAFETY: the adapter caller guarantees initialized scheduler/timer
        // services and IRQ-compatible expiry. The owned timer retains stable
        // callback storage and revalidates routing if the task migrated.
        unsafe {
            self.timer.start(MonotonicInstant::from_span_since_origin(
                TimeSpan::from_nanos(deadline_ns),
            ))
        }
    }

    fn cancel_sync(&mut self) {
        self.timer.cancel_sync();
    }
}

#[kiface::provide]
impl RcuTimerIf {
    fn monotonic_time_nanos() -> u64 {
        khal::time::monotonic_time_nanos()
    }

    fn with_progress_timer(notify_fn: fn(), body_fn: &mut dyn FnMut(&mut dyn ProgressTimer)) {
        let mut alarm = ProgressAlarm {
            timer: Timer::new(move |_: TimerControl<'_>, _: Expiration| notify_fn()),
        };
        body_fn(&mut alarm);
        // Timer's Drop synchronously drains delivery, including on body return.
    }
}

struct TaskParking {
    wait: PreparedTaskWait,
}

impl TaskWait for TaskParking {
    fn waker(&self) -> Waker {
        self.wait.waker()
    }

    fn park(&mut self) {
        self.wait.park();
    }
}

#[kiface::provide]
impl RcuTaskIf {
    fn is_sleepable_context() -> bool {
        ktask::is_sleepable_context()
    }

    fn current_preemption_disabled() -> bool {
        ktask::current_preemption_disabled()
    }

    fn request_current_reschedule() {
        ktask::request_current_reschedule();
    }

    fn check_preempt_pending() {
        ktask::check_preempt_pending();
    }

    fn with_preempt_check_deferred(body_fn: &mut dyn FnMut()) {
        ktask::with_preempt_check_deferred(body_fn);
    }

    fn start_gp_task(entry: PreparedGpTask) {
        let task = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
            move || entry.run(),
            "rcu_gp".into(),
            kbuild_config::TASK_STACK_SIZE,
        ));
        ktask::activate_task(&task);
    }

    fn with_current_reader(body_fn: &mut dyn FnMut(CurrentReader<'_>)) {
        let task = ktask::current();
        // SAFETY: current retains this stack's task borrow through the call,
        // including migration and strictly nested handlers. The body cannot
        // retain this lifetime. RCU, not this adapter, controls scope admission
        // and enrollment cleanup under the reader's additional contracts.
        let reader = unsafe { task.rcu_reader_for_integration() };
        // SAFETY: reader is borrowed from retained current above; the higher-
        // ranked body cannot save this task-local certificate beyond the call.
        let current = unsafe { CurrentReader::new(reader) };
        body_fn(current);
    }

    fn with_prepared_wait(body_fn: &mut dyn FnMut(&mut dyn TaskWait)) -> KResult<()> {
        let mut parking = TaskParking {
            wait: PreparedTaskWait::prepare()?,
        };
        body_fn(&mut parking);
        Ok(())
    }

    fn with_serialized_drain(body_fn: &mut dyn FnMut()) {
        let _guard = RCU_DRAIN_LOCK.lock();
        body_fn();
    }
}

#[kiface::provide]
impl RcuSoftirqIf {
    fn install(action_fn: fn()) -> bool {
        kirq::softirq::open_softirq(kirq::softirq::SoftirqVec::Rcu, action_fn)
    }

    fn raise_local() {
        kirq::softirq::raise_softirq(kirq::softirq::SoftirqVec::Rcu);
    }
}

#[kiface::provide]
impl RcuNotifyIf {
    fn notify_rcu(cpu: LogicalCpuId) -> KResult<()> {
        #[cfg(feature = "ipi")]
        {
            kipi::notify_rcu(cpu).map_err(|_| kerrno::KError::BadState)
        }
        #[cfg(not(feature = "ipi"))]
        {
            if cpu != khal::percpu::this_cpu_id() {
                return Err(kerrno::KError::BadState);
            }
            kirq::softirq::raise_softirq(kirq::softirq::SoftirqVec::Rcu);
            Ok(())
        }
    }

    #[cfg(feature = "ipi")]
    fn notify_reschedule(cpu: LogicalCpuId) -> KResult<()> {
        kipi::notify_reschedule(cpu).map_err(|_| kerrno::KError::BadState)
    }
}

unittest::path_mod_test!(tests_context, "../ut-tests/rcu_context.rs");
unittest::path_mod_test!(rcu_support, "../ut-tests/rcu_support.rs");
unittest::path_mod_test!(tests_owned, "../ut-tests/rcu_owned.rs");
unittest::path_mod_test!(tests_read, "../ut-tests/rcu_read.rs");
unittest::path_mod_test!(tests_cleanup, "../ut-tests/rcu_cleanup.rs");
unittest::path_mod_test!(tests_service, "../ut-tests/rcu_service.rs");
#[cfg(feature = "smp")]
unittest::path_mod_test!(tests_implicit, "../ut-tests/rcu_implicit.rs");
unittest::path_mod_test!(tests_callback, "../ut-tests/rcu_callback.rs");
unittest::path_mod_test!(tests_drain, "../ut-tests/rcu_drain.rs");
unittest::path_mod_test!(tests_service_startup, "../ut-tests/rcu_service_startup.rs");
unittest::path_mod_test!(tests_synchronize, "../ut-tests/rcu_synchronize.rs");
