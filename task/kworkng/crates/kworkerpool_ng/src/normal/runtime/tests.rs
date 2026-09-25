// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;
use crate::SourceRemainder;

struct Empty;

impl crate::PartitionWorkSource for Empty {
    type Execution = Empty;

    fn acquire(&self) -> (Option<Empty>, SourceRemainder) {
        (None, SourceRemainder::Empty)
    }
}

impl crate::AcquiredExecution for Empty {
    fn run(self) {
        panic!("empty provider has no execution");
    }
}

fn start() -> NormalRuntime<Empty> {
    NormalRuntime::start(
        khal::percpu::this_cpu_id(),
        NonZeroUsize::new(1).unwrap(),
        TimeSpan::from_millis(10),
    )
    .unwrap()
}

/// Startup returns with a usable worker; stop joins and removes runtime ownership.
///
/// # Panics
/// Panics on premature readiness, lost baseline, retained storage or incomplete stop.
#[def_test]
fn normal_runtime_waits_for_baseline_and_joins_on_stop() {
    let runtime = start();
    assert!(runtime.service.is_ready());
    assert_eq!(runtime.service.workers.lock().pool.reserved_workers(), 1);
    let weak = Arc::downgrade(&runtime.service);
    let late_wake = runtime.service.notification_waker();
    runtime.stop().unwrap();
    runtime.stop().unwrap();
    assert!(!runtime.service.is_ready());
    assert_eq!(runtime.task.state(), ktask::TaskState::Exited);
    assert_eq!(runtime.service.workers.lock().pool.reserved_workers(), 0);
    assert!(runtime.service.workers.lock().manager_waker.is_none());
    drop(runtime);
    assert!(weak.upgrade().is_none());
    late_wake.wake_by_ref();
}

/// Invalid preparation and stop contexts reject before changing service state.
///
/// # Panics
/// Panics on invalid input acceptance or stop effects from a rejected call.
#[def_test]
fn normal_runtime_rejects_invalid_input_before_effects() {
    let cpu = khal::percpu::this_cpu_id();
    let limit = NonZeroUsize::new(1).unwrap();
    for idle_for in [TimeSpan::MAX, TimeSpan::from_secs(u64::MAX)] {
        assert!(matches!(
            NormalRuntime::<Empty>::start(cpu, limit, idle_for),
            Err(RuntimeError::InvalidIdleInterval)
        ));
    }
    assert!(matches!(
        NormalRuntime::<Empty>::start(cpu, limit, TimeSpan::ZERO),
        Err(RuntimeError::InvalidIdleInterval)
    ));
    assert!(matches!(
        NormalRuntime::<Empty>::start(usize::MAX.into(), limit, TimeSpan::from_millis(1)),
        Err(RuntimeError::CpuUnavailable)
    ));
    let runtime = start();
    {
        let _irq = kspin::NoPreemptIrqSave::new();
        assert!(matches!(
            NormalRuntime::<Empty>::start(cpu, limit, TimeSpan::from_millis(1)),
            Err(RuntimeError::InvalidContext)
        ));
        assert_eq!(runtime.stop(), Err(RuntimeError::InvalidContext));
    }
    assert!(runtime.service.is_ready());
    assert!(!runtime.service.workers.lock().stop_requested);
    runtime.stop().unwrap();
}

/// Rolling back one prepared runtime does not stop an independently owned service.
///
/// # Panics
/// Panics if provider stop affects another service or leaves live helper tasks.
#[def_test]
fn normal_runtime_stop_is_scoped_to_its_owner() {
    let first = start();
    let second = start();
    first.stop().unwrap();
    assert!(second.service.is_ready());
    assert!(!second.service.workers.lock().stop_requested);
    second.stop().unwrap();
}
