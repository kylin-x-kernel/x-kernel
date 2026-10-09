// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Local provider fixtures use kirq's actual softirq runner, not a task-mode drain.

use alloc::{boxed::Box, sync::Arc, task::Wake, vec::Vec};
use core::{
    assert, assert_eq,
    num::NonZeroUsize,
    pin::pin,
    sync::atomic::{AtomicUsize, Ordering},
    task::Waker,
};

use kirq::{
    context::local_bh_disable,
    softirq::{
        self, SoftirqVec,
        test_support::{ScopedSoftirqAction, begin_softirq_test},
    },
};
use kspin::{NoPreempt, SpinNoIrq, static_lock};
use kworkerpool_ng::{BottomHalfRunError, BottomHalfService, PartitionDispatch};
use unittest::def_test;

use super::super::QueuePartitionSource;
use crate::{
    queue::{
        Queue,
        config::{CpuPolicy, QueueConfig},
    },
    work::Work,
};

type Service = BottomHalfService<QueuePartitionSource>;
const VECTOR: SoftirqVec = SoftirqVec::IrqPoll;

mod provider;

fn install_action(action: softirq::SoftirqAction) -> ScopedSoftirqAction {
    assert!(
        !softirq::is_softirq_open(VECTOR),
        "test vector must be unused"
    );
    ScopedSoftirqAction::install(VECTOR, action)
}

static_lock! {
    static ACTIVE: SpinNoIrq<Option<Arc<Service>>> = SpinNoIrq::new(None);
}
static_lock! {
    static WRONG_CPU: SpinNoIrq<Option<Arc<Service>>> = SpinNoIrq::new(None);
}
static PASSES: AtomicUsize = AtomicUsize::new(0);

struct InstalledService;
impl InstalledService {
    fn new(service: Arc<Service>) -> Self {
        let mut slot = ACTIVE.lock();
        assert!(slot.is_none());
        *slot = Some(service);
        PASSES.store(0, Ordering::Relaxed);
        Self
    }
}
impl Drop for InstalledService {
    fn drop(&mut self) {
        let retired = ACTIVE.lock().take();
        drop(retired);
        let retired = WRONG_CPU.lock().take();
        drop(retired);
    }
}

fn action() {
    let service = ACTIVE.lock().as_ref().unwrap().clone();
    PASSES.fetch_add(1, Ordering::Relaxed);
    assert_eq!(service.run_pass(), Ok(()));
}

fn validation_action() {
    let service = ACTIVE.lock().as_ref().unwrap().clone();
    {
        let _irq = kirq::context::test_support::ScopedHardIrqContext::enter();
        assert_eq!(service.run_pass(), Err(BottomHalfRunError::InvalidContext));
    }
    let wrong = WRONG_CPU.lock().as_ref().unwrap().clone();
    assert_eq!(wrong.run_pass(), Err(BottomHalfRunError::WrongCpu));
    assert_eq!(service.run_pass(), Ok(()));
}

// Test-local provider only. Cross-CPU noalloc kick integration is a separate gate.
struct LocalKick(kcpu_id_map::LogicalCpuId);
impl Wake for LocalKick {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        assert_eq!(khal::percpu::this_cpu_id(), self.0);
        softirq::raise_softirq(VECTOR);
    }
}

fn prepare(max_active: usize, kick: Arc<Waker>) -> (Arc<Queue>, Arc<Service>) {
    let cpu = khal::percpu::this_cpu_id();
    let dispatch = PartitionDispatch::new();
    let queue = Queue::with_dispatch(
        QueueConfig::new(&[cpu], CpuPolicy::CurrentCpu, max_active).unwrap(),
        |_| (dispatch.clone(), kick.clone()),
    );
    let service = Arc::new(kworkerpool_ng::test_support::bottom_half_service(
        cpu,
        NonZeroUsize::new(1).unwrap(),
        dispatch,
        kick,
    ));
    (queue, service)
}

/// Actual BH execution refills allowance and requests subsequent softirq passes.
///
/// # Panics
/// Panics on inline callback, wrong context/CPU, lost continuation or duplicate work.
#[def_test(serial)]
fn real_softirq_executes_work_and_completion_refill() {
    let _cpu = NoPreempt::new();
    let _wake_gate = begin_softirq_test();
    let _action = install_action(action);
    let cpu = khal::percpu::this_cpu_id();
    let kick = Arc::new(Waker::from(Arc::new(LocalKick(cpu))));
    let (queue, service) = prepare(1, kick);
    let _installed = InstalledService::new(service);
    let calls = AtomicUsize::new(0);
    let first = pin!(Work::with_callback(|| {
        assert!(kirq::context::is_softirq_context());
        assert_eq!(khal::percpu::this_cpu_id(), cpu);
        calls.fetch_add(1, Ordering::Relaxed);
    }));
    let next = pin!(Work::with_callback(|| {
        assert!(kirq::context::is_softirq_context());
        calls.fetch_add(10, Ordering::Relaxed);
    }));
    let bh = local_bh_disable();
    // SAFETY: pinned Work and captured stack state outlive the complete softirq drain.
    unsafe { queue.submit(&first, Some(cpu), || unreachable!()) }.unwrap();
    // SAFETY: pinned Work and captured stack state outlive the complete softirq drain.
    unsafe { queue.submit(&next, Some(cpu), || unreachable!()) }.unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    drop(bh);
    let _ = softirq::run_pending_softirqs();
    assert_eq!(calls.load(Ordering::Relaxed), 11);
    assert_eq!(PASSES.load(Ordering::Relaxed), 2);
}

/// Demand beyond kirq's restart budget survives for a later runner invocation.
///
/// # Panics
/// Panics on lost pending state, excess callback execution or unbounded BH passes.
#[def_test(serial)]
fn real_softirq_budget_preserves_backlog_across_runner_exit() {
    let _cpu = NoPreempt::new();
    let _wake_gate = begin_softirq_test();
    let _action = install_action(action);
    let cpu = khal::percpu::this_cpu_id();
    let kick = Arc::new(Waker::from(Arc::new(LocalKick(cpu))));
    let (queue, service) = prepare(16, kick);
    let _installed = InstalledService::new(service);
    let calls = AtomicUsize::new(0);
    let works: Vec<_> = (0..13)
        .map(|_| {
            Box::pin(Work::with_callback(|| {
                assert!(kirq::context::is_softirq_context());
                calls.fetch_add(1, Ordering::Relaxed);
            }))
        })
        .collect();
    // Keep IRQs masked across the bounded observation, so timer IRQ-tail drains
    // cannot consume the residual bit before we assert that it was retained.
    let irq = kspin::NoPreemptIrqSave::new();
    let bh = local_bh_disable();
    for work in &works {
        // SAFETY: all Box-pinned Work/captures remain live until all 13 complete.
        unsafe {
            queue.submit(work, Some(cpu), || unreachable!()).unwrap();
        }
    }
    drop(bh);
    let first = calls.load(Ordering::Relaxed);
    assert!(first > 0 && first < 13);
    assert_ne!(softirq::local_softirq_pending(), 0);
    // No new submission is needed to finish the residual source demand.
    for _ in 0..13 {
        let _ = softirq::run_pending_softirqs();
        if calls.load(Ordering::Relaxed) == 13 {
            break;
        }
    }
    assert_eq!(calls.load(Ordering::Relaxed), 13);
    assert_eq!(PASSES.load(Ordering::Relaxed), 13);
    drop(irq);
}

/// Wrong execution contexts must not acquire any pending Work.
///
/// # Panics
/// Panics on effectful rejection or failure to execute after a valid BH entry.
#[def_test(serial)]
fn bh_service_rejects_task_hardirq_and_wrong_cpu_before_claim() {
    let _cpu = NoPreempt::new();
    let _wake_gate = begin_softirq_test();
    let _action = install_action(validation_action);
    let cpu = khal::percpu::this_cpu_id();
    let (queue, service) = prepare(1, Arc::new(Waker::noop().clone()));
    let _installed = InstalledService::new(service.clone());
    let calls = AtomicUsize::new(0);
    let work = pin!(Work::with_callback(|| {
        calls.fetch_add(1, Ordering::Relaxed);
    }));
    // SAFETY: the valid softirq entry below consumes the sole accepted occurrence.
    unsafe {
        queue.submit(&work, Some(cpu), || unreachable!()).unwrap();
    }
    assert_eq!(service.run_pass(), Err(BottomHalfRunError::InvalidContext));
    {
        let _bh = local_bh_disable();
        assert_eq!(service.run_pass(), Err(BottomHalfRunError::InvalidContext));
    }
    {
        let _irq = kirq::context::test_support::ScopedHardIrqContext::enter();
        assert_eq!(service.run_pass(), Err(BottomHalfRunError::InvalidContext));
    }
    let dispatch = PartitionDispatch::new();
    let wrong = kworkerpool_ng::test_support::bottom_half_service(
        kcpu_id_map::LogicalCpuId::new(cpu.as_usize() + 1),
        NonZeroUsize::new(1).unwrap(),
        dispatch,
        Arc::new(Waker::noop().clone()),
    );
    *WRONG_CPU.lock() = Some(Arc::new(wrong));
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    softirq::raise_softirq(VECTOR);
    let _ = softirq::run_pending_softirqs();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}
