// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use krcu_core::RcuContextIf;
use unittest::{assert, assert_eq, def_test};

use super::{IRQ_STATE, IrqLine, IrqState, IrqStateDesc, MsiReservation, PreparedWithdrawal};
use crate::{
    IrqDesc, IrqFlags, IrqRef,
    domain::TEST_DOMAIN,
    runtime::{index, rcu::with_nmi_context},
};

pub(super) struct IndexObserver {
    virq: usize,
    visits: usize,
    is_valid: bool,
}

impl IrqState {
    pub(super) fn observe_index_transition(&mut self, line: &Arc<IrqLine>, entry: &IrqStateDesc) {
        if self
            .index_observer
            .as_ref()
            .is_some_and(|probe| Some(probe.virq) == line.logical_irq())
        {
            let is_valid = observe_transition(line, entry);
            if let Some(probe) = self.index_observer.as_mut() {
                probe.visits += 1;
                probe.is_valid &= is_valid;
            }
        }
    }
}

pub(super) fn with_irq<R>(body: impl FnOnce() -> R) -> R {
    let _pin = kspin::NoPreemptIrqSave::new();
    // SAFETY: initialized test task, pinned CPU and IRQ exclusion span this
    // balanced entry; observers neither schedule nor leave a read scope open.
    let entry = unsafe { RcuContextIf::current_cpu_watching().enter_interrupt() };
    let irq = crate::context::HardIrqContextGuard::enter();
    let result = body();
    drop(irq);
    // SAFETY: all nested reads/entries have ended under the same CPU exclusion.
    unsafe { entry.exit() };
    result
}

fn nested_translation(hwirq: usize, expected: Option<usize>) -> bool {
    let observe = || {
        let outer = crate::translate_hwirq(TEST_DOMAIN, hwirq);
        // SAFETY: strictly nested NMI with the same exclusion and no scheduling.
        let inner = unsafe { with_nmi_context(|| crate::translate_hwirq(TEST_DOMAIN, hwirq)) };
        outer == expected && inner == expected && !krcu::is_reading()
    };
    // SAFETY: the observer runs inside the pinned IRQ fixture, returns on this
    // stack and invokes only scalar RCU queries, never Line locks or callbacks.
    unsafe { with_nmi_context(observe) }
}

fn observe_transition(line: &Arc<IrqLine>, entry: &IrqStateDesc) -> bool {
    let virq = line.logical_irq().unwrap();
    let watching = RcuContextIf::current_cpu_watching();
    let depth = watching.interrupt_depth_for_test();
    let was_reading = krcu::is_reading();
    // This fixture runs only with actual SMP lock exclusion. A partial lookup
    // must not acquire the Line lock used by dispatch to admit a handler.
    let is_excluded = line.state.try_lock().is_none();
    let valid = with_irq(|| {
        let domain = index::lookup(IrqRef::Domain(TEST_DOMAIN, entry.desc.hwirq));
        let logical = index::lookup(IrqRef::Virq(virq)).and_then(|(_, line)| line);
        let matches = |found: &Arc<IrqLine>| Arc::ptr_eq(found, line);
        let indexes_match = if entry.is_retired() {
            domain.is_none() && logical.as_ref().is_some_and(matches)
        } else {
            domain
                .and_then(|(_, line)| line)
                .as_ref()
                .is_some_and(matches)
                && logical.is_none()
        };
        let expected = (!entry.is_retired()).then_some(virq);
        indexes_match && nested_translation(entry.desc.hwirq, expected)
    });
    is_excluded
        && valid
        && watching.interrupt_depth_for_test() == depth
        && krcu::is_reading() == was_reading
        && !kiface_irq_context_api::InterruptContextIf::is_in_interrupt_handler()
}

#[def_test(serial)]
fn partial_publication_and_withdrawal_keep_admission_closed() {
    // UP SpinNoIrq has no lock word. Do not pretend its try_lock proves
    // cross-CPU exclusion, or create aliased guards on an elided lock.
    let lock = kspin::SpinRaw::new(());
    let has_smp_exclusion = {
        let _guard = lock.lock();
        lock.is_locked()
    };
    if !has_smp_exclusion {
        return unittest::TestResult::Ignored;
    }
    let virq = 0x5fc2;
    let hwirq = 0x5fc3;
    IRQ_STATE.lock().index_observer = Some(IndexObserver {
        virq,
        visits: 0,
        is_valid: true,
    });
    let desc = IrqDesc::from_hwirq(hwirq)
        .with_domain(TEST_DOMAIN)
        .with_virq(virq)
        .with_flags(IrqFlags::MSI);
    let binding = crate::runtime::manager::tests_irq::SyntheticLine::mapped(desc);
    let (visits, valid) = {
        let state = IRQ_STATE.lock();
        let probe = state.index_observer.as_ref().unwrap();
        (probe.visits, probe.is_valid)
    };
    assert_eq!(binding.virq(), virq);
    assert_eq!(visits, 1);
    assert!(valid);
    let line = IRQ_STATE.lock().descs.get(&virq).unwrap();
    line.lock().msi_reservation = Some(MsiReservation::for_test(hwirq));
    let prepared = PreparedWithdrawal::new(hwirq).unwrap();
    line.lock().msi_withdrawal = Some(prepared);
    krcu::preflight_callback_submission().unwrap();
    let withdrawn = {
        let mut state = IRQ_STATE.lock();
        // SAFETY: preflight completed before exclusion; both records are
        // submitted immediately after unlocking, before consuming the reservation.
        unsafe { state.withdraw_msi(virq) }
    }
    .unwrap();
    withdrawn.2.retire();
    let probe = IRQ_STATE.lock().index_observer.take().unwrap();
    // Backend ownership is still ours, despite one index already being absent
    // during the checkpoint. No backend can reuse a retained reservation.
    assert_eq!(withdrawn.1.hwirq(), hwirq);
    assert_eq!(probe.visits, 2);
    assert!(probe.is_valid);
    assert!(crate::descriptor(virq).is_none());
    assert_eq!(crate::translate_hwirq(TEST_DOMAIN, hwirq), None);
    drop(withdrawn.1);
    drop(withdrawn.0);
    drop(line);
    // SAFETY: no reader, guard or callback-needed resource remains held.
    unsafe { krcu::drain_callbacks() }.unwrap();
}
