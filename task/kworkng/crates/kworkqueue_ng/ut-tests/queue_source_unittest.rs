// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, task::Wake};
use core::{
    assert, assert_eq,
    num::NonZeroUsize,
    pin::pin,
    sync::atomic::{AtomicUsize, Ordering},
    task::Waker,
};

use kspin::SpinNoIrq;
use unittest::def_test;

use super::*;
use crate::{
    queue::config::{CpuPolicy, QueueConfig},
    work::{
        Work,
        observers::{CompletionObserver, PreparedObserver},
    },
};

#[path = "queue_source_bottom_half_unittest.rs"]
mod bottom_half;
#[path = "queue_source_normal_unittest.rs"]
mod normal;

/// Shutdown removes stale canceled demand and completes its shared source receipt.
///
/// # Panics
/// Panics on premature detachment, retained demand or non-idempotent shutdown.
#[def_test]
fn shutdown_detaches_canceled_source_and_repeats() {
    let (queue, dispatch, _) = setup(1);
    let work = pin!(Work::with_callback(|| panic!("canceled work ran")));
    let receipt = queue
        .partition(0.into())
        .unwrap()
        .source_detachment_completion()
        .unwrap();
    // SAFETY: pinned Work stays live through synchronous cancellation and drain.
    unsafe { queue.submit(&work, Some(0.into()), || unreachable!()) }.unwrap();
    assert!(work.cancel_pending());
    {
        let _guard = kspin::NoPreemptIrqSave::new();
        assert_eq!(
            queue.close_and_drain(),
            Err(crate::work::wait::WaitError::InvalidContext)
        );
    }
    assert!(!receipt.is_completed());
    assert!(queue.admission_color().is_some());
    queue.close_and_drain().unwrap();
    assert!(receipt.is_completed());
    assert!(!dispatch.run_one_turn());
    queue.close_and_drain().unwrap();
}

/// Every concurrent shutdown waits for an already-selected source inspection.
///
/// # Panics
/// Panics on early receipt completion, failed joins or residual dispatch demand.
#[def_test]
fn concurrent_shutdowns_wait_for_source_inspection_retirement() {
    use core::sync::atomic::AtomicBool;

    let (queue, dispatch, _) = setup(1);
    let work = pin!(Work::with_callback(|| panic!("canceled work ran")));
    // SAFETY: pinned Work survives cancellation and all joined inspection/drain calls.
    unsafe { queue.submit(&work, Some(0.into()), || unreachable!()) }.unwrap();
    assert!(work.cancel_pending());
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let inspector = ktask::spawn({
        let (dispatch, entered, release) = (dispatch.clone(), entered.clone(), release.clone());
        move || {
            kworkerpool_ng::test_support::with_source_inspection(&dispatch, || {
                entered.store(true, Ordering::Release);
                while !release.load(Ordering::Acquire) {
                    ktask::yield_now();
                }
            })
        }
    });
    while !entered.load(Ordering::Acquire) {
        ktask::yield_now();
    }
    let drains = core::array::from_fn::<_, 2, _>(|_| {
        let queue = queue.clone();
        ktask::spawn(move || queue.close_and_drain().unwrap())
    });
    while drains
        .iter()
        .any(|task| task.state() != ktask::TaskState::Blocked)
    {
        ktask::yield_now();
    }
    let receipt = queue
        .partition(0.into())
        .unwrap()
        .source_detachment_completion()
        .unwrap();
    assert!(!receipt.is_completed());
    release.store(true, Ordering::Release);
    assert_eq!(inspector.join(), 0);
    for task in drains {
        assert_eq!(task.join(), 0);
    }
    assert!(receipt.is_completed());
    assert!(!dispatch.run_one_turn());
}

struct CountWake(AtomicUsize);

impl Wake for CountWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

fn config(max_active: usize) -> QueueConfig {
    QueueConfig::validate(
        &[LogicalCpuId::new(0)],
        CpuPolicy::CurrentCpu,
        max_active,
        |_| true,
    )
    .unwrap()
}

fn setup(
    max_active: usize,
) -> (
    Arc<Queue>,
    Arc<PartitionDispatch<QueuePartitionSource>>,
    Arc<CountWake>,
) {
    let dispatch = PartitionDispatch::new();
    let wakes = Arc::new(CountWake(AtomicUsize::new(0)));
    let kick = Arc::new(Waker::from(wakes.clone()));
    let queue = Queue::with_dispatch(config(max_active), |_| (dispatch.clone(), kick.clone()));
    (queue, dispatch, wakes)
}

/// Acceptance and completion refill both drive the actual dispatcher.
///
/// # Panics
/// Panics on missing demand, inline execution or incorrect allowance refill.
#[def_test]
fn dispatch_runs_real_work_and_refills_without_new_submission() {
    let (queue, dispatch, wakes) = setup(1);
    let calls = AtomicUsize::new(0);
    let first = pin!(Work::with_callback(|| {
        calls.fetch_add(1, Ordering::Relaxed);
    }));
    let next = pin!(Work::with_callback(|| {
        calls.fetch_add(10, Ordering::Relaxed);
    }));
    // SAFETY: pinned Work and stack captures outlive all executions below.
    unsafe { queue.submit(&first, None, || LogicalCpuId::new(0)) }.unwrap();
    // SAFETY: pinned Work and stack captures outlive all executions below.
    unsafe { queue.submit(&next, None, || LogicalCpuId::new(0)) }.unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert_eq!(wakes.0.load(Ordering::Relaxed), 1);
    assert!(dispatch.run_bounded_pass(NonZeroUsize::new(1).unwrap()));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(wakes.0.load(Ordering::Relaxed), 2);
    assert!(!dispatch.run_bounded_pass(NonZeroUsize::new(1).unwrap()));
    assert_eq!(calls.load(Ordering::Relaxed), 11);
}

/// Canceling a ready item returns its reservation and publishes its replacement.
///
/// # Panics
/// Panics if cancellation loses demand or lets the canceled callback execute.
#[def_test]
fn ready_cancellation_publishes_waiting_work() {
    let (queue, dispatch, wakes) = setup(1);
    let calls = AtomicUsize::new(0);
    let canceled = pin!(Work::with_callback(|| panic!("canceled callback")));
    let next = pin!(Work::with_callback(|| {
        calls.fetch_add(1, Ordering::Relaxed);
    }));
    // SAFETY: canceled storage is unlinked and next is run before leaving scope.
    unsafe { queue.submit(&canceled, None, || LogicalCpuId::new(0)) }.unwrap();
    // SAFETY: canceled storage is unlinked and next is run before leaving scope.
    unsafe { queue.submit(&next, None, || LogicalCpuId::new(0)) }.unwrap();
    assert!(canceled.cancel_pending());
    assert_eq!(wakes.0.load(Ordering::Relaxed), 2);
    assert!(dispatch.run_one_turn());
    assert!(!dispatch.run_one_turn());
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

/// A running predecessor's successor becomes demand only after retirement.
///
/// # Panics
/// Panics on premature demand, duplicate execution or missing successor service.
#[def_test]
fn blocked_successor_gets_a_fresh_notice_after_retirement() {
    let (queue, dispatch, wakes) = setup(2);
    let calls = AtomicUsize::new(0);
    let work = pin!(Work::with_callback(|| {
        calls.fetch_add(1, Ordering::Relaxed);
    }));
    // SAFETY: both accepted occurrences finish before pinned Work is destroyed.
    unsafe {
        queue.submit(&work, None, || LogicalCpuId::new(0)).unwrap();
    }
    let running = queue.claim_with_demand(LogicalCpuId::new(0)).0.unwrap();
    // SAFETY: same external storage and drain obligation as the predecessor.
    unsafe {
        queue.submit(&work, None, || LogicalCpuId::new(0)).unwrap();
    }
    assert_eq!(wakes.0.load(Ordering::Relaxed), 1);
    assert!(
        dispatch.run_one_turn(),
        "consume stale demand while predecessor runs"
    );
    assert!(!dispatch.run_one_turn());
    running.run();
    assert_eq!(wakes.0.load(Ordering::Relaxed), 2);
    assert!(dispatch.run_one_turn());
    assert!(!dispatch.run_one_turn());
    assert_eq!(calls.load(Ordering::Relaxed), 2);
}

/// A claim remains valid after both the source and external Queue handle leave.
///
/// # Panics
/// Panics on premature Queue release, leaked ownership or callback loss.
#[def_test]
fn concrete_claim_survives_source_and_queue_handle_release() {
    let queue = Queue::new(config(1));
    let dispatch = PartitionDispatch::new();
    let weak = Arc::downgrade(&queue);
    let source = dispatch.create_source(QueuePartitionSource {
        queue: weak.clone(),
        cpu: LogicalCpuId::new(0),
    });
    let weak_source = Arc::downgrade(&source);
    let calls = AtomicUsize::new(0);
    let work = pin!(Work::with_callback(|| {
        assert!(
            weak_source.upgrade().is_none(),
            "inspection retired before callback"
        );
        assert!(
            weak.upgrade().is_some(),
            "running resources still retain Queue"
        );
        calls.fetch_add(1, Ordering::Relaxed);
    }));
    // SAFETY: external pinned Work and captures remain live beyond run below.
    unsafe {
        queue.submit(&work, None, || LogicalCpuId::new(0)).unwrap();
    }
    dispatch.notify(&source);
    drop(source);
    drop(queue);
    assert!(weak.upgrade().is_some());
    assert!(dispatch.run_one_turn());
    assert!(weak.upgrade().is_none());
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(!dispatch.run_one_turn());
}

/// A queued notice cannot retain a drained Queue or dereference its former Work.
///
/// # Panics
/// Panics on a strong-reference cycle or callback after cancellation.
#[def_test]
fn canceled_last_work_leaves_only_harmless_stale_demand() {
    let (queue, dispatch, _) = setup(1);
    let weak = Arc::downgrade(&queue);
    {
        let work = pin!(Work::with_callback(|| panic!("canceled callback")));
        // SAFETY: cancel removes the only accepted occurrence before storage leaves.
        unsafe {
            queue.submit(&work, None, || LogicalCpuId::new(0)).unwrap();
        }
        assert!(work.cancel_pending());
    }
    drop(queue);
    assert!(weak.upgrade().is_none());
    assert!(dispatch.run_one_turn());
    assert!(!dispatch.run_one_turn());
}

/// Two queue sources share a dispatcher without sharing admission/allowance.
///
/// # Panics
/// Panics on source starvation or accepted-work loss following closure.
#[def_test]
fn shared_dispatch_rotates_sources_and_drains_closed_queue() {
    let (first_queue, dispatch, wakes) = setup(1);
    let kick = Arc::new(Waker::from(wakes));
    let second_queue = Queue::with_dispatch(config(1), |_| (dispatch.clone(), kick.clone()));
    let trace = AtomicUsize::new(0);
    let first = pin!(Work::with_callback(|| {
        trace.fetch_add(1, Ordering::Relaxed);
    }));
    let second = pin!(Work::with_callback(|| {
        trace.fetch_add(10, Ordering::Relaxed);
    }));
    // SAFETY: both queues are drained before the external Work/captures leave.
    unsafe { first_queue.submit(&first, None, || LogicalCpuId::new(0)) }.unwrap();
    // SAFETY: both queues are drained before the external Work/captures leave.
    unsafe { second_queue.submit(&second, None, || LogicalCpuId::new(0)) }.unwrap();
    first_queue.close_admission();
    assert!(dispatch.run_one_turn());
    assert_eq!(trace.load(Ordering::Relaxed), 1);
    assert!(dispatch.run_one_turn());
    assert_eq!(trace.load(Ordering::Relaxed), 11);
    assert!(!dispatch.run_one_turn());
}

// Test-only Weak locator probes the management lock without retaining Queue.
struct LockProbe {
    queue: SpinNoIrq<Weak<Queue>>,
    token: SpinNoIrq<Option<PreparedObserver>>,
    calls: AtomicUsize,
}

/// Each sparse CPU route publishes to its own prepared dispatcher and kick.
///
/// # Panics
/// Panics on wrong-service publication, cross-CPU claim or duplicate execution.
#[def_test]
fn sparse_cpu_bindings_keep_demand_and_execution_separate() {
    let dispatches = [PartitionDispatch::new(), PartitionDispatch::new()];
    let wakes = [
        Arc::new(CountWake(AtomicUsize::new(0))),
        Arc::new(CountWake(AtomicUsize::new(0))),
    ];
    let config = QueueConfig::validate(
        &[LogicalCpuId::new(0), LogicalCpuId::new(2)],
        CpuPolicy::CurrentCpu,
        1,
        |_| true,
    )
    .unwrap();
    let queue = Queue::with_dispatch(config, |cpu| {
        let index = cpu.as_usize() / 2;
        (
            dispatches[index].clone(),
            Arc::new(Waker::from(wakes[index].clone())),
        )
    });
    let calls = AtomicUsize::new(0);
    let zero = pin!(Work::with_callback(|| {
        calls.fetch_add(1, Ordering::Relaxed);
    }));
    let two = pin!(Work::with_callback(|| {
        calls.fetch_add(10, Ordering::Relaxed);
    }));
    // SAFETY: both pinned Work objects and their captures outlive both turns.
    unsafe { queue.submit(&zero, Some(LogicalCpuId::new(0)), || unreachable!()) }.unwrap();
    // SAFETY: both pinned Work objects and their captures outlive both turns.
    unsafe { queue.submit(&two, Some(LogicalCpuId::new(2)), || unreachable!()) }.unwrap();
    assert_eq!(wakes[0].0.load(Ordering::Relaxed), 1);
    assert_eq!(wakes[1].0.load(Ordering::Relaxed), 1);
    assert!(dispatches[1].run_one_turn());
    assert_eq!(calls.load(Ordering::Relaxed), 10);
    assert!(!dispatches[1].run_one_turn());
    assert!(dispatches[0].run_one_turn());
    assert_eq!(calls.load(Ordering::Relaxed), 11);
    assert!(!dispatches[0].run_one_turn());
}

impl Wake for LockProbe {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        let queue = self.queue.lock().upgrade().unwrap();
        let token = self.token.lock().take().unwrap();
        // Color 3 has no admissions; crossing the barrier must not deadlock.
        let token = queue
            .partition(LogicalCpuId::new(0))
            .unwrap()
            .observe_batch(3, token)
            .err()
            .unwrap();
        *self.token.lock() = Some(token);
        self.calls.fetch_add(1, Ordering::Relaxed);
    }
}

/// Provider wake runs unlocked on acceptance, cancellation, refill and timer transitions.
///
/// # Panics
/// Panics on missing kicks or unexpected color use; held locks cause test timeout.
#[def_test]
fn service_kicks_run_after_partition_unlock() {
    let (_, token) = CompletionObserver::prepare();
    let probe = Arc::new(LockProbe {
        queue: SpinNoIrq::new(Weak::new()),
        token: SpinNoIrq::new(Some(token)),
        calls: AtomicUsize::new(0),
    });
    let dispatch = PartitionDispatch::new();
    let kick = Arc::new(Waker::from(probe.clone()));
    let queue = Queue::with_dispatch(config(1), |_| (dispatch.clone(), kick.clone()));
    *probe.queue.lock() = Arc::downgrade(&queue);
    let canceled = pin!(Work::with_callback(|| {}));
    let first = pin!(Work::with_callback(|| {}));
    let next = pin!(Work::with_callback(|| {}));
    // SAFETY: all pinned objects are canceled/executed before scope exit.
    unsafe { queue.submit(&canceled, None, || LogicalCpuId::new(0)) }.unwrap();
    // SAFETY: all pinned objects are canceled/executed before scope exit.
    unsafe { queue.submit(&first, None, || LogicalCpuId::new(0)) }.unwrap();
    // SAFETY: all pinned objects are canceled/executed before scope exit.
    unsafe { queue.submit(&next, None, || LogicalCpuId::new(0)) }.unwrap();
    assert!(canceled.cancel_pending());
    assert!(dispatch.run_one_turn());
    assert!(dispatch.run_one_turn());
    assert!(!dispatch.run_one_turn());
    assert_eq!(probe.calls.load(Ordering::Relaxed), 3);
    let timer = pin!(Work::with_callback(|| {}));
    let partition = queue.partition(0.into()).unwrap();
    partition.install_delayed_for_test(&queue, &timer, 10);
    // SAFETY: pinned fixture storage remains live through dispatch and cleanup below.
    assert_eq!(unsafe { partition.queue_due(&timer, 10) }, Ok(true));
    assert_eq!(probe.calls.load(Ordering::Relaxed), 4);
    assert!(dispatch.run_one_turn());
    assert!(!dispatch.run_one_turn());

    let deferred = pin!(crate::work::delayed::DelayedWork::new(|| {
        panic!("withdrawn callback ran")
    }));
    // SAFETY: deferred and next remain pinned through ordinary execution and
    // physical timer drain; initialized boot service is not replaced by this test.
    unsafe {
        deferred
            .as_ref()
            .queue_after_on(&queue, 0.into(), ktime_types::TimeSpan::ZERO)
    }
    .unwrap();
    // SAFETY: deferred and next remain pinned through ordinary execution and
    // physical timer drain; initialized boot service is not replaced by this test.
    unsafe { queue.submit(&next, Some(0.into()), || unreachable!()) }.unwrap();
    assert_eq!(probe.calls.load(Ordering::Relaxed), 5);
    let (withdrawn, token) = CompletionObserver::prepare();
    let waiter = ktask::spawn({
        let (withdrawn, kick) = (withdrawn.clone(), kick.clone());
        move || {
            withdrawn.wait(&ktask::completion::CompletionWait::prepare().unwrap());
            kick.wake_by_ref();
        }
    });
    while waiter.state() != ktask::TaskState::Blocked {
        ktask::yield_now();
    }
    deferred
        .work_for_test()
        .capture(&mut alloc::vec![token])
        .unwrap();
    let future = ktime_types::MonotonicInstant::from_span_since_origin(
        ktime_types::TimeSpan::from_nanos(u64::MAX),
    );
    // SAFETY: retained pinned storage outlives withdrawal, refill and synchronized cancel.
    assert!(unsafe { deferred.as_ref().retime_pending_at(future) }.unwrap());
    assert_eq!(waiter.join(), 0);
    assert!(withdrawn.is_completed());
    // Both the service refill and old Work observer reenter the partition unlocked.
    assert_eq!(probe.calls.load(Ordering::Relaxed), 7);
    assert!(dispatch.run_one_turn());
    assert!(!dispatch.run_one_turn());
    assert!(deferred.cancel_sync().unwrap());
}
