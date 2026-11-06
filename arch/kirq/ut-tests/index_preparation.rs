// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kspin::{SpinNoIrq, static_lock};
use unittest::{assert, assert_eq, def_test};

use super::IrqDescError;
use crate::{IrqDesc, domain::TEST_DOMAIN, runtime::state::IRQ_STATE};

struct Failure {
    virq: usize,
    key: usize,
    calls: usize,
}

static_lock! {
    static PREPARATION_FAILURE: SpinNoIrq<Option<Failure>> = SpinNoIrq::new(None);
}
static_lock! {
    static RETIREMENT_FAILURE: SpinNoIrq<Option<Failure>> = SpinNoIrq::new(None);
}

pub(super) fn check_retirement_preparation(hwirq: usize, record: usize) -> Option<()> {
    if let Some(failure) = RETIREMENT_FAILURE.lock().as_mut()
        && failure.virq == hwirq
    {
        failure.calls += 1;
        if failure.key == record {
            return None;
        }
    }
    Some(())
}

struct FailRetirement;

impl Drop for FailRetirement {
    fn drop(&mut self) {
        *RETIREMENT_FAILURE.lock() = None;
    }
}

#[def_test(serial)]
fn msi_retirement_storage_failure_returns_unpublished_reservation() {
    let hwirq = 0x5fc6;
    let desc = IrqDesc::from_hwirq(hwirq)
        .with_domain(TEST_DOMAIN)
        .with_flags(crate::IrqFlags::MSI);
    for record in [0, 1] {
        *RETIREMENT_FAILURE.lock() = Some(Failure {
            virq: hwirq,
            key: record,
            calls: 0,
        });
        let injection = FailRetirement;
        let result = crate::runtime::state::publish_msi_reservation(
            desc,
            crate::backend::msi::MsiReservation::for_test(hwirq),
        );
        let calls = RETIREMENT_FAILURE.lock().as_ref().unwrap().calls;
        drop(injection);
        let (error, reservation) = result.unwrap_err();
        assert!(matches!(error, IrqDescError::NoMemory));
        assert_eq!(reservation.hwirq(), hwirq);
        assert_eq!(calls, record + 1);
        assert_eq!(crate::translate_hwirq(TEST_DOMAIN, hwirq), None);
        // The production allocation caller returns this exact reservation to
        // its backend on Err; no published resource has escaped to a driver.
        drop(reservation);
    }
}

#[cfg(target_arch = "x86_64")]
#[def_test(serial)]
fn allocated_msi_release_needs_no_new_retirement_storage_and_can_retry() {
    let allocation = crate::alloc_msix(crate::IrqAffinity::Any).unwrap();
    let virq = allocation.virq();
    let desc = crate::descriptor(virq).unwrap();
    let handler = alloc::sync::Arc::new(|_| crate::IrqEvent::HANDLED);
    assert!(crate::try_register_disabled(virq, handler).unwrap());
    *RETIREMENT_FAILURE.lock() = Some(Failure {
        virq: desc.hwirq,
        key: 0,
        calls: 0,
    });
    let injection = FailRetirement;
    let rejected = !crate::free_msix(virq);
    let retained = crate::descriptor(virq).is_some();
    let removed = crate::try_free_irq(virq).unwrap();
    let released = crate::free_msix(virq);
    let calls = RETIREMENT_FAILURE.lock().as_ref().unwrap().calls;
    drop(injection);
    assert!(rejected && retained && removed.is_some() && released);
    assert_eq!(calls, 0);
    assert!(crate::descriptor(virq).is_none());
    assert_eq!(crate::translate_hwirq(crate::MSI_DOMAIN, desc.hwirq), None);
}

struct InjectFailure;

impl Drop for InjectFailure {
    fn drop(&mut self) {
        *PREPARATION_FAILURE.lock() = None;
    }
}

pub(super) fn check_preparation(virq: usize, key: usize) -> Result<(), IrqDescError> {
    if let Some(failure) = PREPARATION_FAILURE.lock().as_mut()
        && failure.virq == virq
    {
        failure.calls += 1;
        if failure.key == key {
            return Err(IrqDescError::NoMemory);
        }
    }
    Ok(())
}

#[def_test(serial)]
fn either_index_preparation_failure_leaves_mapping_unpublished_and_retryable() {
    let virq = 0x5fc0;
    let hwirq = 0x5fc1;
    let desc = IrqDesc::from_hwirq(hwirq)
        .with_domain(TEST_DOMAIN)
        .with_virq(virq);
    for (key, expected_calls) in [(virq, 1), (hwirq, 2)] {
        *PREPARATION_FAILURE.lock() = Some(Failure {
            virq,
            key,
            calls: 0,
        });
        let injection = InjectFailure;
        let result = crate::try_map(desc);
        let calls = PREPARATION_FAILURE.lock().as_ref().unwrap().calls;
        drop(injection);
        assert!(matches!(result, Err(IrqDescError::NoMemory)));
        assert_eq!(calls, expected_calls);
        assert!(crate::descriptor(virq).is_none());
        assert_eq!(crate::translate_hwirq(TEST_DOMAIN, hwirq), None);
        let binding = crate::runtime::manager::tests_irq::SyntheticLine::mapped(desc);
        assert_eq!(binding.virq(), virq);
        assert_eq!(crate::translate_hwirq(TEST_DOMAIN, hwirq), Some(virq));
        // This synthetic key has no device or concurrent borrowed reader.
        let removed = IRQ_STATE.lock().remove_test_line(virq);
        assert!(removed.is_some());
        drop(removed);
    }
}
