// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kspin::{SpinNoIrq, static_lock};
use unittest::{assert, assert_eq, def_test};

use super::PreparedAction;
use crate::{
    Handler, IrqDesc, IrqDescError, IrqEvent, IrqHandler,
    runtime::state::{IRQ_STATE, IrqLine, IrqStateDesc},
};

struct Failure {
    target: Weak<dyn IrqHandler>,
    calls: Arc<AtomicUsize>,
}

static_lock! {
    static ALLOCATION_FAILURE: SpinNoIrq<Option<Failure>> = SpinNoIrq::new(None);
}

pub(super) fn reject_allocation(handler: &Handler) -> bool {
    let failure = ALLOCATION_FAILURE.lock();
    if let Some(failure) = failure.as_ref()
        && core::ptr::addr_eq(failure.target.as_ptr(), Arc::as_ptr(handler))
    {
        failure.calls.fetch_add(1, Ordering::Relaxed);
        return true;
    }
    false
}

struct FailureScope(Option<Failure>);

impl FailureScope {
    fn new(handler: &Handler, calls: Arc<AtomicUsize>) -> Self {
        Self(ALLOCATION_FAILURE.lock().replace(Failure {
            target: Arc::downgrade(handler),
            calls,
        }))
    }
}

impl Drop for FailureScope {
    fn drop(&mut self) {
        let removed = core::mem::replace(&mut *ALLOCATION_FAILURE.lock(), self.0.take());
        drop(removed);
    }
}

struct Cleanup(usize);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let removed = IRQ_STATE.lock().remove_test_line(self.0);
        drop(removed);
    }
}

struct OfferedHandler {
    line: Weak<IrqLine>,
    drops: Arc<AtomicUsize>,
    has_unlocked_drop: Arc<AtomicBool>,
}

impl IrqHandler for OfferedHandler {
    fn handle(&self, _: usize) -> IrqEvent {
        IrqEvent::HANDLED
    }
}

impl Drop for OfferedHandler {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Release);
        // An accidentally retained short guard leaves IRQs masked. Do not
        // reenter locks in that case; report the invalid destructor context.
        if !karch::local_irq_enabled() || crate::context::is_in_interrupt_context() {
            return;
        }
        let Some(line) = self.line.upgrade() else {
            return;
        };
        drop(IRQ_STATE.lock());
        let has_management_access = line.management.try_lock().is_some();
        drop(line.lock());
        self.has_unlocked_drop
            .store(has_management_access, Ordering::Release);
    }
}

#[derive(Clone, Copy)]
enum Registration {
    Regular,
    Startup,
    Shared,
}

fn check_allocation_failure(kind: Registration, is_enabled: bool) -> unittest::TestResult {
    let virq = 0x5fd8;
    assert!(!IRQ_STATE.lock().descs.contains_key(&virq));
    let desc = IrqDesc::from_virq(virq);
    let mut entry = IrqStateDesc::new(desc);
    let survivor_calls = Arc::new(AtomicUsize::new(0));
    let observed = survivor_calls.clone();
    let survivor: Handler = Arc::new(move |_| {
        observed.fetch_add(1, Ordering::Relaxed);
        IrqEvent::HANDLED
    });
    let first_token = if matches!(kind, Registration::Shared) {
        let mut prepared = PreparedAction::shared(survivor.clone()).unwrap();
        Some(entry.publish_shared_action(&mut prepared).unwrap())
    } else {
        None
    };
    // Synthetic Line: model delivery state without invoking a real irqchip.
    let plan = if is_enabled {
        entry.prepare_auto_enable()
    } else {
        entry.prepare_register_disabled()
    };
    entry.commit_platform_plan(plan);
    entry.is_pending = true;
    let line = Arc::new(IrqLine::new(entry));
    assert!(IRQ_STATE.lock().descs.insert(virq, line.clone()).is_none());
    let _cleanup = Cleanup(virq);
    let state = || {
        let entry = line.lock();
        (
            entry.desc,
            entry.generation_for_tests(),
            entry.configured_generation_for_tests(),
            entry.action_count(),
            entry.is_enabled_for_tests(),
            entry.disable_depth_for_tests(),
            entry.is_pending,
            entry.is_teardown_in_progress(),
        )
    };
    let before = state();
    let calls = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let has_unlocked_drop = Arc::new(AtomicBool::new(false));
    let offered: Handler = Arc::new(OfferedHandler {
        line: Arc::downgrade(&line),
        drops: drops.clone(),
        has_unlocked_drop: has_unlocked_drop.clone(),
    });
    let richer = desc
        .with_source(crate::IrqSource::Acpi)
        .with_flags(crate::IrqFlags::WAKEUP_SOURCE);
    let injection = FailureScope::new(&offered, calls.clone());
    let failed = match kind {
        Registration::Regular => crate::try_register_disabled(richer, offered).map(|_| ()),
        Registration::Startup => {
            crate::try_register_startup(richer, offered, crate::IrqRegisterOptions::disabled())
                .map(|_| ())
        }
        Registration::Shared => crate::try_register_shared(richer, offered).map(|_| ()),
    };
    drop(injection);
    assert_eq!(failed, Err(IrqDescError::NoMemory));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert!(has_unlocked_drop.load(Ordering::Acquire));
    assert_eq!(state(), before);
    assert_eq!(crate::descriptor(virq), Some(desc));
    let snapshot = line.lock().snapshot_actions();
    let mut seen = 0;
    let mut has_valid_survivor = true;
    snapshot.with_actions(|action| {
        has_valid_survivor &=
            Some(action.token()) == first_token && action.run_primary(virq).handled();
        seen += 1;
    });
    assert!(has_valid_survivor);
    assert_eq!(seen, usize::from(first_token.is_some()));
    assert_eq!(survivor_calls.load(Ordering::Relaxed), seen);
    assert_eq!(Arc::strong_count(&survivor), 1 + seen);
    // A later successful publication must use the immediately next sequence;
    // failed regular preparation must not consume one either.
    let mut retry = PreparedAction::shared(Arc::new(|_| IrqEvent::HANDLED)).unwrap();
    let next = line.lock().publish_shared_action(&mut retry).unwrap();
    assert_eq!(next.id(), first_token.map_or(1, |token| token.id() + 1));
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn regular_action_allocation_failure_preserves_state_and_unlocked_drop() -> unittest::TestResult {
    for kind in [Registration::Regular, Registration::Startup] {
        for is_enabled in [false, true] {
            let result = check_allocation_failure(kind, is_enabled);
            if !result.is_ok() {
                return result;
            }
        }
    }
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn shared_action_allocation_failure_preserves_survivor_and_next_token() -> unittest::TestResult {
    for is_enabled in [false, true] {
        let result = check_allocation_failure(Registration::Shared, is_enabled);
        if !result.is_ok() {
            return result;
        }
    }
    unittest::TestResult::Ok
}
