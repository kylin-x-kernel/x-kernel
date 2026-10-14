// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Isolated real Work/softirq integration, not global workng activation.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    assert, assert_eq,
    num::NonZeroUsize,
    sync::atomic::{AtomicUsize, Ordering},
};

use kcpu_id_map::LogicalCpuId;
use kirq::softirq::{SoftirqVec, test_support::ScopedSoftirqAction};
use kspin::{SpinNoIrq, static_lock};
use kworkerpool_ng::BottomHalfPriority;
use unittest::def_test;

use super::{CpuPolicy, Queue, QueueConfig, Service, Work};

static_lock! {
    static SERVICE: SpinNoIrq<Option<Arc<Service>>> = SpinNoIrq::new(None);
}
static PASSES: AtomicUsize = AtomicUsize::new(0);

fn action() {
    // Late entries after unpublication are harmless. An already acquired service
    // reference retains this pass until Fixture::drop observes its release.
    let service = SERVICE.lock().clone();
    if let Some(service) = service {
        PASSES.fetch_add(1, Ordering::Relaxed);
        assert_eq!(service.run_pass(), Ok(()));
    }
}

struct Fixture {
    service: Arc<Service>,
    _action: ScopedSoftirqAction,
}

impl Fixture {
    fn new(cpu: LogicalCpuId, priority: BottomHalfPriority) -> Self {
        let service = Arc::new(Service::new(cpu, priority, NonZeroUsize::new(1).unwrap()).unwrap());
        let vector = match priority {
            BottomHalfPriority::Normal => SoftirqVec::Tasklet,
            BottomHalfPriority::High => SoftirqVec::High,
        };
        PASSES.store(0, Ordering::Relaxed);
        assert!(SERVICE.lock().replace(service.clone()).is_none());
        Self {
            service,
            _action: ScopedSoftirqAction::install(vector, action),
        }
    }

    fn queue(&self, cpu: LogicalCpuId) -> Arc<Queue> {
        Queue::with_dispatch(
            QueueConfig::new(&[cpu], CpuPolicy::CurrentCpu, 1).unwrap(),
            |_| (self.service.dispatch(), self.service.notification_waker()),
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Tests stop producers and drain accepted Work first. Unpublish before
        // joining pass references; a copied softirq action cannot acquire anew.
        let retired = SERVICE.lock().take();
        drop(retired);
        while Arc::strong_count(&self.service) != 1 {
            core::hint::spin_loop();
        }
    }
}

fn exercise(priority: BottomHalfPriority, select_cpu: fn(LogicalCpuId) -> LogicalCpuId) {
    // Keep target selection and IRQ submissions on the same sender. Preparation
    // here is allocation-capable test setup, outside spinlocks and IRQ masking.
    let sender = kspin::NoPreempt::new();
    let cpu = select_cpu(khal::percpu::this_cpu_id());
    let fixture = Fixture::new(cpu, priority);
    let queue = fixture.queue(cpu);
    let calls = AtomicUsize::new(0);
    let works: Vec<_> = (0..13)
        .map(|_| {
            Box::pin(Work::with_callback(|| {
                assert!(kirq::context::is_softirq_context());
                assert_eq!(khal::percpu::this_cpu_id(), cpu);
                calls.fetch_add(1, Ordering::Relaxed);
            }))
        })
        .collect();
    {
        let _irq = kspin::NoPreemptIrqSave::new();
        for work in &works {
            // SAFETY: Box pins each externally owned Work; callbacks borrow calls
            // and cpu until close_and_drain completes all callbacks and cleanup.
            unsafe {
                queue.submit(work, Some(cpu), || unreachable!()).unwrap();
            }
        }
    }
    drop(sender);
    // Do not manually drive softirq or claim. IRQ exit/ksoftirqd must make progress,
    // including completion refill and continuation beyond kirq's restart budget.
    queue.close_and_drain().unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 13);
    assert!(PASSES.load(Ordering::Relaxed) >= 13);
    drop(works);
    drop(queue);
    drop(fixture);
}

/// Prepared local notification drives both BH priorities without a manual runner.
///
/// Serial transport integration requires quiescent production Tasklet/High queues.
/// Scoped registration is restored after all Work and service passes retire.
///
/// # Panics
/// Panics on wrong context/CPU, incomplete cleanup, or unexpected preparation failure.
#[def_test(serial)]
fn prepared_local_bh_notification_runs_work_and_refill() {
    exercise(BottomHalfPriority::Normal, |local| local);
    exercise(BottomHalfPriority::High, |local| local);
}

/// Remote prepared submission reaches the selected CPU through fixed IPI reasons.
///
/// # Panics
/// Panics on wrong context/CPU, missing Work cleanup or duplicate execution.
#[def_test(serial)]
fn prepared_remote_bh_notification_runs_work_and_refill() {
    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    exercise(BottomHalfPriority::Normal, remote_cpu);
    exercise(BottomHalfPriority::High, remote_cpu);
}

fn remote_cpu(local: LogicalCpuId) -> LogicalCpuId {
    let mut remote = None;
    kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| {
        if cpu != local && remote.is_none() {
            remote = Some(cpu);
        }
    });
    remote.expect("test requires two present CPUs")
}

/// Invalid target preparation fails before any dispatcher or notification exposure.
///
/// # Panics
/// Panics if an absent CPU is accepted.
#[def_test]
fn prepared_bh_service_rejects_absent_cpu() {
    assert!(
        Service::new(
            LogicalCpuId::new(usize::MAX),
            BottomHalfPriority::Normal,
            NonZeroUsize::new(1).unwrap()
        )
        .is_err()
    );
}
