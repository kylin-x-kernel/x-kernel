// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::def_test;

use crate::{
    IrqDesc, IrqFlow, IrqRef,
    action::IrqAction,
    state::{IRQ_STATE, IrqLine, IrqStateDesc},
};

static MASKS: AtomicUsize = AtomicUsize::new(0);
static UNMASKS: AtomicUsize = AtomicUsize::new(0);
static CHIP: crate::LevelChip = crate::LevelChip {
    mask_ack: |_| {
        MASKS.fetch_add(1, Ordering::Relaxed);
    },
    unmask: |_| {
        UNMASKS.fetch_add(1, Ordering::Relaxed);
    },
};

struct Fixture {
    line: Arc<IrqLine>,
    virq: usize,
}
impl Fixture {
    fn new() -> Self {
        MASKS.store(0, Ordering::Relaxed);
        UNMASKS.store(0, Ordering::Relaxed);
        let virq = 0x6e90;
        let mut entry = IrqStateDesc::new(IrqDesc::from_virq(virq));
        entry.flow = IrqFlow::Level(&CHIP);
        let line = Arc::new(IrqLine::new(entry));
        assert!(IRQ_STATE.lock().descs.insert(virq, line.clone()).is_none());
        Self { line, virq }
    }

    fn enable(&self) {
        let mut entry = self.line.lock();
        let plan = entry.prepare_enable_irq();
        entry.commit_platform_plan(plan);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        IRQ_STATE.lock().descs.remove(&self.virq);
    }
}

#[def_test(serial)]
fn level_overlap_masks_and_completes_each_claim_without_recursive_actions() {
    let fixture = Fixture::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let called = calls.clone();
    let weak = Arc::downgrade(&fixture.line);
    let virq = fixture.virq;
    assert!(
        fixture
            .line
            .lock()
            .install_regular_action(IrqAction::regular(Arc::new(move |_| {
                assert_eq!(MASKS.load(Ordering::Relaxed), 1);
                assert_eq!(UNMASKS.load(Ordering::Relaxed), 0);
                called.fetch_add(1, Ordering::Relaxed);
                super::dispatch_claim(IrqRef::Virq(virq), || {
                    assert_eq!(MASKS.load(Ordering::Relaxed), 2);
                });
                let line = weak.upgrade().unwrap();
                assert!(line.lock().is_running);
                assert!(line.lock().is_pending);
                assert_eq!(line.lock().in_flight(), 1);
                crate::IrqEvent::HANDLED
            })))
    );
    fixture.enable();
    super::dispatch_claim(IrqRef::Virq(virq), || {
        assert_eq!(MASKS.load(Ordering::Relaxed), 1);
    });
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(UNMASKS.load(Ordering::Relaxed), 1);
    let entry = fixture.line.lock();
    assert!(!entry.is_pending);
    assert!(!entry.is_running);
    assert!(entry.in_flight_zero_completion().is_completed());
}

#[def_test(serial)]
fn level_disabled_arrival_and_handler_disable_never_unmask_at_tail() {
    let fixture = Fixture::new();
    let weak = Arc::downgrade(&fixture.line);
    let calls = Arc::new(AtomicUsize::new(0));
    let called = calls.clone();
    assert!(
        fixture
            .line
            .lock()
            .install_regular_action(IrqAction::regular(Arc::new(move |_| {
                called.fetch_add(1, Ordering::Relaxed);
                let line = weak.upgrade().unwrap();
                let mut entry = line.lock();
                let plan = entry.prepare_disable_irq_nosync();
                entry.commit_platform_plan(plan);
                crate::IrqEvent::HANDLED
            })))
    );
    let mut completed = 0;
    super::dispatch_claim(IrqRef::Virq(fixture.virq), || {
        completed += 1;
    });
    assert_eq!(completed, 1);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert!(fixture.line.lock().is_pending);
    fixture.enable();
    super::super::resend::schedule(&fixture.line);
    assert!(!fixture.line.lock().is_pending);
    assert!(!fixture.line.lock().is_replay_queued);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    super::dispatch_claim(IrqRef::Virq(fixture.virq), || {
        completed += 1;
    });
    assert_eq!(completed, 2);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(UNMASKS.load(Ordering::Relaxed), 0);
    assert_eq!(fixture.line.lock().in_flight(), 0);
}
