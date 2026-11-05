// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use unittest::def_test;

use crate::{
    IrqDesc, IrqFlow, IrqRef,
    action::IrqAction,
    state::{IRQ_STATE, IrqLine, IrqStateDesc},
};

static MASKS: AtomicUsize = AtomicUsize::new(0);
static UNMASKS: AtomicUsize = AtomicUsize::new(0);
static REQUESTS: AtomicUsize = AtomicUsize::new(0);
static ACCEPTS: AtomicBool = AtomicBool::new(true);
static CHIP: crate::EdgeChip = crate::EdgeChip {
    mask: |_| {
        MASKS.fetch_add(1, Ordering::Relaxed);
    },
    unmask: |_| {
        UNMASKS.fetch_add(1, Ordering::Relaxed);
    },
    retrigger: |_| {
        REQUESTS.fetch_add(1, Ordering::Relaxed);
        ACCEPTS.load(Ordering::Relaxed)
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
        REQUESTS.store(0, Ordering::Relaxed);
        ACCEPTS.store(true, Ordering::Relaxed);
        let virq = 0x6e80;
        let mut entry = IrqStateDesc::new(IrqDesc::from_virq(virq));
        entry.flow = IrqFlow::Edge(&CHIP);
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
fn overlapping_edges_are_acknowledged_once_and_coalesced_by_running_owner() {
    let fixture = Fixture::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let acks = Arc::new(AtomicUsize::new(0));
    let called = calls.clone();
    let acknowledged = acks.clone();
    let weak = Arc::downgrade(&fixture.line);
    let virq = fixture.virq;
    assert!(
        fixture
            .line
            .lock()
            .install_regular_action(IrqAction::regular(Arc::new(move |_| {
                assert!(krcu::is_reading());
                let call = called.fetch_add(1, Ordering::Relaxed);
                let line = weak.upgrade().unwrap();
                assert!(line.lock().is_running);
                if call == 0 {
                    assert_eq!(acknowledged.load(Ordering::Relaxed), 1);
                    for _ in 0..2 {
                        super::dispatch_claim(IrqRef::Virq(virq), || {
                            assert!(MASKS.load(Ordering::Relaxed) > 0);
                            acknowledged.fetch_add(1, Ordering::Relaxed);
                        });
                    }
                    assert_eq!(called.load(Ordering::Relaxed), 1);
                    assert!(line.lock().is_pending);
                    assert_eq!(line.lock().in_flight(), 1);
                } else {
                    assert_eq!(call, 1);
                    assert_eq!(acknowledged.load(Ordering::Relaxed), 3);
                    assert!(!line.lock().is_pending);
                }
                crate::IrqEvent::HANDLED
            })))
    );
    fixture.enable();
    let completion = fixture.line.lock().in_flight_zero_completion();
    super::dispatch_claim(IrqRef::Virq(virq), || {
        assert!(!krcu::is_reading());
        acks.fetch_add(1, Ordering::Relaxed);
    });
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert_eq!(acks.load(Ordering::Relaxed), 3);
    assert_eq!(MASKS.load(Ordering::Relaxed), 2);
    assert_eq!(UNMASKS.load(Ordering::Relaxed), 1);
    assert_eq!(REQUESTS.load(Ordering::Relaxed), 0);
    assert!(!fixture.line.lock().is_running);
    assert!(completion.is_completed());
}

#[def_test(serial)]
fn disabled_edge_records_pending_until_successful_hardware_request() {
    let fixture = Fixture::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let called = calls.clone();
    assert!(
        fixture
            .line
            .lock()
            .install_regular_action(IrqAction::regular(Arc::new(move |_| {
                called.fetch_add(1, Ordering::Relaxed);
                crate::IrqEvent::HANDLED
            })))
    );
    let mut acks = 0;
    super::dispatch_claim(IrqRef::Virq(fixture.virq), || {
        acks += 1;
    });
    assert_eq!(acks, 1);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert!(fixture.line.lock().is_pending);
    super::super::resend::schedule(&fixture.line);
    assert_eq!(REQUESTS.load(Ordering::Relaxed), 0);
    fixture.enable();
    ACCEPTS.store(false, Ordering::Relaxed);
    super::super::resend::schedule(&fixture.line);
    assert!(fixture.line.lock().is_pending);
    assert!(!fixture.line.lock().is_replay_queued);
    assert_eq!(fixture.line.lock().in_flight(), 0);
    ACCEPTS.store(true, Ordering::Relaxed);
    super::super::resend::schedule(&fixture.line);
    super::super::resend::schedule(&fixture.line);
    assert_eq!(REQUESTS.load(Ordering::Relaxed), 2);
    assert!(!fixture.line.lock().is_pending);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    super::dispatch_claim(IrqRef::Virq(fixture.virq), || {
        acks += 1;
    });
    assert_eq!(acks, 2);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[def_test(serial)]
fn handler_disable_keeps_overlapping_edge_pending_without_tail_unmask() {
    let fixture = Fixture::new();
    let weak = Arc::downgrade(&fixture.line);
    let virq = fixture.virq;
    let calls = Arc::new(AtomicUsize::new(0));
    let called = calls.clone();
    assert!(
        fixture
            .line
            .lock()
            .install_regular_action(IrqAction::regular(Arc::new(move |_| {
                called.fetch_add(1, Ordering::Relaxed);
                let line = weak.upgrade().unwrap();
                {
                    let mut entry = line.lock();
                    let plan = entry.prepare_disable_irq_nosync();
                    entry.commit_platform_plan(plan);
                }
                super::dispatch_claim(IrqRef::Virq(virq), || {});
                crate::IrqEvent::HANDLED
            })))
    );
    fixture.enable();
    super::dispatch_claim(IrqRef::Virq(virq), || {});
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(UNMASKS.load(Ordering::Relaxed), 0);
    assert!(fixture.line.lock().is_pending);
    assert!(!fixture.line.lock().is_running);
    assert!(!fixture.line.lock().can_run_actions());
    assert!(
        fixture
            .line
            .lock()
            .in_flight_zero_completion()
            .is_completed()
    );
}
