// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use unittest::def_test;

use super::*;

fn cpus() -> Vec<LogicalCpuId> {
    let mut cpus = Vec::new();
    kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| cpus.push(cpu));
    cpus
}

fn budget() -> NonZeroUsize {
    NonZeroUsize::new(2).unwrap()
}

/// Every boot CPU receives independent normal/high dispatchers and notifications.
///
/// # Panics
/// Panics on missing coverage, shared dispatchers, wrong placement or retained storage.
#[def_test]
fn prepared_bh_services_cover_both_priorities_on_every_cpu() {
    let services = BottomHalfServices::<()>::prepare(budget()).unwrap();
    let present = cpus();
    assert_eq!(services.cpus.len(), present.len());
    let mut dispatches = Vec::new();
    let mut notifications = Vec::new();
    for cpu in present {
        let normal = services.service(cpu, BottomHalfPriority::Normal).unwrap();
        let high = services.service(cpu, BottomHalfPriority::High).unwrap();
        assert!(!Arc::ptr_eq(&normal.dispatch, &high.dispatch));
        assert!(!Arc::ptr_eq(&normal.kick, &high.kick));
        for service in [normal, high] {
            assert_eq!(service.cpu, cpu);
            assert_eq!(service.budget, budget());
            dispatches.push(Arc::downgrade(&service.dispatch));
            notifications.push(Arc::downgrade(&service.kick));
        }
    }
    assert!(
        services
            .service(usize::MAX.into(), BottomHalfPriority::Normal)
            .is_none()
    );
    drop(services);
    assert!(dispatches.iter().all(|weak| weak.upgrade().is_none()));
    assert!(notifications.iter().all(|weak| weak.upgrade().is_none()));
}

/// Failure at either priority drops completed CPUs and the incomplete current pair.
///
/// # Panics
/// Panics on leaked service storage, wrong failure or damage to an unrelated set.
#[def_test]
fn prepared_bh_services_release_every_failed_prefix() {
    let present = cpus();
    let unrelated = BottomHalfServices::<()>::prepare(budget()).unwrap();
    for failed in 0..present.len() * 2 {
        let mut attempts = 0;
        let mut dispatches = Vec::new();
        let mut notifications = Vec::new();
        let result = BottomHalfServices::prepare_with(&present, |cpu, priority| {
            let index = attempts;
            attempts += 1;
            if index == failed {
                return Err(PrepareError::CpuUnavailable);
            }
            let service = BottomHalfService::<()>::new(cpu, priority, budget()).unwrap();
            dispatches.push(Arc::downgrade(&service.dispatch));
            notifications.push(Arc::downgrade(&service.kick));
            Ok(service)
        });
        assert!(matches!(result, Err(PrepareError::CpuUnavailable)));
        assert_eq!(attempts, failed + 1);
        assert_eq!(dispatches.len(), failed);
        assert!(dispatches.iter().all(|weak| weak.upgrade().is_none()));
        assert!(notifications.iter().all(|weak| weak.upgrade().is_none()));
        assert!(
            unrelated
                .service(present[0], BottomHalfPriority::High)
                .is_some()
        );
    }
}

/// Empty CPU coverage and nonsleepable preparation cannot expose a partial set.
///
/// # Panics
/// Panics on accepted invalid context or preparation invoked for an empty set.
#[def_test]
fn prepared_bh_services_reject_invalid_preparation() {
    assert!(matches!(
        BottomHalfServices::<()>::prepare_with(&[], |_, _| unreachable!()),
        Err(PrepareError::CpuUnavailable)
    ));
    let _irq = kspin::NoPreemptIrqSave::new();
    assert!(matches!(
        BottomHalfServices::<()>::prepare(budget()),
        Err(PrepareError::InvalidContext)
    ));
}

/// Single and collection preparation share the public CPU-validation error.
///
/// # Panics
/// Panics if an absent CPU is accepted or the two preparation paths disagree.
#[def_test]
fn prepared_bh_service_reports_public_cpu_error() {
    for priority in [BottomHalfPriority::Normal, BottomHalfPriority::High] {
        let result: Result<BottomHalfService<()>, crate::BottomHalfPrepareError> =
            BottomHalfService::new(usize::MAX.into(), priority, budget());
        assert!(matches!(
            result,
            Err(crate::BottomHalfPrepareError::CpuUnavailable)
        ));
    }
    let result = BottomHalfServices::<()>::prepare_with(&[usize::MAX.into()], |cpu, priority| {
        BottomHalfService::new(cpu, priority, budget())
    });
    assert!(matches!(
        result,
        Err(crate::BottomHalfPrepareError::CpuUnavailable)
    ));
}
