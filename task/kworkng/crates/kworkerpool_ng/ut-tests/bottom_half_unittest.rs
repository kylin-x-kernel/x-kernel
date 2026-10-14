// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Standalone BH service contracts through the real softirq runner.

use alloc::{sync::Arc, task::Wake};
use core::sync::atomic::{AtomicUsize, Ordering};

use kirq::softirq::{
    self, SoftirqVec,
    test_support::{ScopedSoftirqAction, begin_softirq_test},
};
use kspin::{SpinNoIrq, static_lock};
use unittest::def_test;

use super::*;
use crate::{AcquiredExecution, SourceRemainder};

struct Probe {
    pending: AtomicUsize,
    completed: AtomicUsize,
}

impl PartitionWorkSource for Arc<Probe> {
    type Execution = Self;

    fn acquire(&self) -> (Option<Self>, SourceRemainder) {
        match self
            .pending
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
        {
            Ok(n) => (
                Some(self.clone()),
                if n > 1 {
                    SourceRemainder::More
                } else {
                    SourceRemainder::Empty
                },
            ),
            Err(_) => (None, SourceRemainder::Empty),
        }
    }
}

impl AcquiredExecution for Arc<Probe> {
    fn run(self) {
        assert!(kirq::context::is_softirq_context());
        self.completed.fetch_add(1, Ordering::Release);
    }
}

struct CountWake(AtomicUsize);

impl Wake for CountWake {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

type Service = BottomHalfService<Arc<Probe>>;
static_lock! {
    static ACTIVE: SpinNoIrq<Option<Arc<Service>>> = SpinNoIrq::new(None);
}

struct Installed;

impl Drop for Installed {
    fn drop(&mut self) {
        let retired = ACTIVE.lock().take();
        drop(retired);
    }
}

fn action() {
    let service = ACTIVE.lock().as_ref().unwrap().clone();
    {
        let _hardirq = kirq::context::test_support::ScopedHardIrqContext::enter();
        assert_eq!(service.run_pass(), Err(BottomHalfRunError::InvalidContext));
    }
    let wrong = Service::with_dispatch(
        usize::MAX.into(),
        service.budget,
        service.dispatch(),
        service.notification_waker(),
    );
    assert_eq!(wrong.run_pass(), Err(BottomHalfRunError::WrongCpu));
    assert_eq!(service.run_pass(), Ok(()));
}

/// Invalid contexts acquire nothing; each real BH pass honors its turn budget.
///
/// # Panics
/// Panics on incorrect context rejection, callback count or continuation count.
#[def_test(serial)]
fn bh_service_rejects_invalid_context_and_retains_budget_continuation() {
    let _cpu = kspin::NoPreempt::new();
    let cpu = khal::percpu::this_cpu_id();
    let dispatch = PartitionDispatch::new();
    let wake = Arc::new(CountWake(AtomicUsize::new(0)));
    let service = Arc::new(Service::with_dispatch(
        cpu,
        NonZeroUsize::new(2).unwrap(),
        dispatch.clone(),
        Arc::new(Waker::from(wake.clone())),
    ));
    let probe = Arc::new(Probe {
        pending: AtomicUsize::new(3),
        completed: AtomicUsize::new(0),
    });
    let source = dispatch.create_source(probe.clone());
    dispatch.notify(&source);
    assert_eq!(service.run_pass(), Err(BottomHalfRunError::InvalidContext));
    {
        let _bh = kirq::context::local_bh_disable();
        assert_eq!(service.run_pass(), Err(BottomHalfRunError::InvalidContext));
    }
    assert_eq!(probe.completed.load(Ordering::Acquire), 0);

    // Exclude IRQ-tail drains while checking exact per-pass observations.
    let _irq = kspin::NoPreemptIrqSave::new();
    assert_eq!(khal::percpu::this_cpu_id(), cpu);
    let _gate = begin_softirq_test();
    assert!(!softirq::is_softirq_open(SoftirqVec::IrqPoll));
    let _action = ScopedSoftirqAction::install(SoftirqVec::IrqPoll, action);
    assert!(ACTIVE.lock().is_none());
    *ACTIVE.lock() = Some(service);
    let _installed = Installed;
    softirq::raise_softirq_irqoff(SoftirqVec::IrqPoll);
    let _ = softirq::run_pending_softirqs();
    assert_eq!(probe.completed.load(Ordering::Acquire), 2);
    assert_eq!(wake.0.load(Ordering::Relaxed), 1);
    // Explicitly deliver the retained continuation; the test Waker only counts it.
    softirq::raise_softirq_irqoff(SoftirqVec::IrqPoll);
    let _ = softirq::run_pending_softirqs();
    assert_eq!(probe.completed.load(Ordering::Acquire), 3);
    assert_eq!(wake.0.load(Ordering::Relaxed), 1);
    dispatch.detach(&source);
    assert!(source.detachment_completion().is_completed());
}
