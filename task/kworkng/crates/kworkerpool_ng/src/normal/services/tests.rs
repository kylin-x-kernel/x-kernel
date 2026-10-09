// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

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
    #[expect(
        clippy::panic,
        reason = "CG-021-no-panic-on-reachable-failure: test scenario: an empty work source never \
                  yields an execution to run"
    )]
    fn run(self) {
        panic!("empty source cannot execute");
    }
}

fn limit() -> NonZeroUsize {
    NonZeroUsize::new(1).unwrap()
}
fn idle() -> TimeSpan {
    TimeSpan::from_millis(10)
}
fn cpus() -> Vec<LogicalCpuId> {
    let mut cpus = Vec::new();
    kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| cpus.push(cpu));
    cpus
}

/// Successful construction covers every actual boot CPU before returning.
///
/// # Panics
/// Panics on missing/misrouted services, partial stop or retained service ownership.
#[def_test]
fn normal_services_cover_boot_cpus_and_join_all() {
    let services = NormalServices::<Empty>::start(limit(), idle()).unwrap();
    let present = cpus();
    assert_eq!(services.runtimes.len(), present.len());
    let weak: Vec<_> = services
        .runtimes
        .iter()
        .map(|runtime| Arc::downgrade(&runtime.service))
        .collect();
    for &cpu in &present {
        let service = services.service(cpu).unwrap();
        assert_eq!(service.cpu, cpu);
        assert!(service.is_ready());
        let (dispatch, kick) = services.binding(cpu).unwrap();
        assert!(Arc::ptr_eq(&dispatch, &service.dispatch));
        assert!(Arc::ptr_eq(&kick, &service.notification_waker()));
    }
    assert!(services.service(usize::MAX.into()).is_none());
    assert!(services.binding(usize::MAX.into()).is_none());
    {
        let _irq = kspin::NoPreemptIrqSave::new();
        assert_eq!(services.stop(), Err(RuntimeError::InvalidContext));
    }
    assert!(
        present
            .iter()
            .all(|&cpu| services.service(cpu).unwrap().is_ready())
    );
    services.stop().unwrap();
    services.stop().unwrap();
    assert!(
        present
            .iter()
            .all(|&cpu| !services.service(cpu).unwrap().is_ready())
    );
    drop(services);
    assert!(weak.iter().all(|service| service.upgrade().is_none()));
}

/// Every failed startup prefix is joined without stopping an unrelated runtime.
///
/// # Panics
/// Panics on wrong error, partial service escape, retained tasks or cross-owner stop.
#[def_test]
fn normal_services_roll_back_each_failed_startup_prefix() {
    let present = cpus();
    let unrelated = NormalRuntime::<Empty>::start(present[0], limit(), idle()).unwrap();
    for failed in 0..present.len() {
        let mut weak = Vec::new();
        let mut attempted = 0;
        let result = NormalServices::start_with(&present, |cpu| {
            let index = attempted;
            attempted += 1;
            if index == failed {
                return Err(RuntimeError::CpuUnavailable);
            }
            let runtime = NormalRuntime::<Empty>::start(cpu, limit(), idle())?;
            weak.push(Arc::downgrade(&runtime.service));
            Ok(runtime)
        });
        assert!(matches!(result, Err(RuntimeError::CpuUnavailable)));
        assert_eq!(attempted, failed + 1);
        assert_eq!(weak.len(), failed);
        assert!(weak.iter().all(|service| service.upgrade().is_none()));
        assert!(unrelated.service.is_ready());
    }
    unrelated.stop().unwrap();
}

/// Invalid collection input/context cannot activate a partial CPU set.
///
/// # Panics
/// Panics on invalid input acceptance or invoking a starter for empty coverage.
#[def_test]
fn normal_services_reject_before_startup() {
    for idle_for in [TimeSpan::MAX, TimeSpan::from_secs(u64::MAX)] {
        assert!(matches!(
            NormalServices::<Empty>::start(limit(), idle_for),
            Err(RuntimeError::InvalidIdleInterval)
        ));
    }
    assert!(matches!(
        NormalServices::<Empty>::start(limit(), TimeSpan::ZERO),
        Err(RuntimeError::InvalidIdleInterval)
    ));
    assert!(matches!(
        NormalServices::<Empty>::start_with(&[], |_| unreachable!()),
        Err(RuntimeError::CpuUnavailable)
    ));
    let _irq = kspin::NoPreemptIrqSave::new();
    assert!(matches!(
        NormalServices::<Empty>::start(limit(), idle()),
        Err(RuntimeError::InvalidContext)
    ));
}
