// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{collections::BTreeMap, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use block::completion::BlockCompletionOperations;
use device_res::{IrqHandlerToken, IrqTrigger, ResError};
use kirq::{
    context::test_support::ScopedHardIrqContext, softirq::test_support::ScopedDaemonWakeGate,
};
use ksync::{Mutex, static_lock};
use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::block_completion::prepare_block_wait;

const FAILED_IRQ: usize = 0x4003;
struct MockProvider;
static PROVIDER: MockProvider = MockProvider;
static NEXT_TOKEN: AtomicUsize = AtomicUsize::new(1);
static_lock! {
    static ACTIONS: Mutex<BTreeMap<(usize, usize), Arc<dyn IrqHandler>>> =
        Mutex::new(BTreeMap::new());
}

impl IrqOp for MockProvider {
    fn request_irq(
        &self,
        irq: IrqResource,
        handler: Arc<dyn IrqHandler>,
    ) -> ResResult<IrqHandlerToken> {
        if irq.number == FAILED_IRQ {
            return Err(ResError::Busy);
        }
        let id = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
        core::assert!(ACTIONS.lock().insert((irq.number, id), handler).is_none());
        Ok(IrqHandlerToken::shared_action(id))
    }
    fn release_irq(&self, irq: IrqResource, token: IrqHandlerToken) {
        let IrqHandlerToken::SharedAction(id) = token else {
            panic!("expected shared action");
        };
        core::assert!(ACTIONS.lock().remove(&(irq.number, id)).is_some());
    }
    fn set_irq_enabled(&self, _irq: IrqResource, _enabled: bool) {
        panic!("block removal must not mask a shared line");
    }
}

struct TestDevice {
    is_claimed: AtomicBool,
    calls: AtomicUsize,
    reclaims: AtomicUsize,
}

impl TestDevice {
    fn new(is_claimed: bool) -> Arc<Self> {
        Arc::new(Self {
            is_claimed: AtomicBool::new(is_claimed),
            calls: AtomicUsize::new(0),
            reclaims: AtomicUsize::new(0),
        })
    }
}

impl IrqHandler for TestDevice {
    fn handle(&self, _irq: usize) -> IrqEvent {
        core::assert!(kirq::context::is_in_hardirq());
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.is_claimed.load(Ordering::Relaxed) {
            IrqEvent::from_sources(0b101)
        } else {
            IrqEvent::NOT_HANDLED
        }
    }
}

impl BlockCompletionOperations for TestDevice {
    fn process_completed_requests(&self) {
        self.reclaims.fetch_add(1, Ordering::Relaxed);
    }
}

fn resource(irq: usize) -> IrqResource {
    IrqResource::new(irq, IrqTrigger::LevelHigh)
}

fn subscribe(device: &Arc<TestDevice>, irq: usize) -> (Irq, Arc<BlockIoReclaimer>) {
    let completion: Arc<dyn BlockCompletionOperations> = device.clone();
    let reclaimer = BlockIoReclaimer::new(Arc::downgrade(&completion));
    let handler: Arc<dyn IrqHandler> = device.clone();
    let registration = request_block_irq(
        &PROVIDER,
        resource(irq),
        Arc::downgrade(&handler),
        reclaimer.clone(),
    )
    .expect("test IRQ registration must succeed");
    (registration, reclaimer)
}

fn dispatch(irq: usize) -> IrqEvent {
    let handlers: Vec<_> = ACTIONS
        .lock()
        .range((irq, 0)..=(irq, usize::MAX))
        .map(|(_, handler)| handler.clone())
        .collect();
    let _context = ScopedHardIrqContext::enter();
    let mut event = IrqEvent::NOT_HANDLED;
    for handler in handlers {
        event.merge(handler.handle(irq));
    }
    event
}

fn stop_reclaimer(reclaimer: Arc<BlockIoReclaimer>) {
    let mut waiter = prepare_block_wait().expect("test block wait preparation must succeed");
    reclaimer.stop_and_wait(waiter.as_mut());
}

#[def_test(serial)]
fn block_irq_registers_each_device_and_releases_only_its_action() {
    let _wake_gate = ScopedDaemonWakeGate::disabled();
    let first = TestDevice::new(true);
    let second = TestDevice::new(false);
    let other = TestDevice::new(true);
    let (first_irq, first_reclaimer) = subscribe(&first, 0x4000);
    let (second_irq, second_reclaimer) = subscribe(&second, 0x4000);
    let (other_irq, other_reclaimer) = subscribe(&other, 0x4001);
    assert_eq!(ACTIONS.lock().len(), 3);
    assert_eq!(dispatch(0x4000), IrqEvent::from_sources(0b101));
    kirq::softirq::run_pending_softirqs();
    assert_eq!(first.reclaims.load(Ordering::Relaxed), 1);
    assert_eq!(second.reclaims.load(Ordering::Relaxed), 0);
    assert_eq!(other.calls.load(Ordering::Relaxed), 0);
    drop(first_irq);
    assert_eq!(ACTIONS.lock().len(), 2);
    assert!(!dispatch(0x4000).handled());
    assert_eq!(first.calls.load(Ordering::Relaxed), 1);
    assert_eq!(second.calls.load(Ordering::Relaxed), 2);
    drop(second_irq);
    assert_eq!(ACTIONS.lock().len(), 1);
    assert!(dispatch(0x4001).handled());
    kirq::softirq::run_pending_softirqs();
    drop(other_irq);
    stop_reclaimer(first_reclaimer);
    stop_reclaimer(second_reclaimer);
    stop_reclaimer(other_reclaimer);
    assert!(ACTIONS.lock().is_empty());
}

#[def_test(serial)]
fn block_irq_propagates_provider_failure_without_retaining_callback() {
    let reclaimer = BlockIoReclaimer::new(Weak::<TestDevice>::new());
    let failed = request_block_irq(
        &PROVIDER,
        resource(FAILED_IRQ),
        Weak::<TestDevice>::new(),
        reclaimer.clone(),
    );
    assert!(matches!(failed, Err(ResError::Busy)));
    assert!(ACTIONS.lock().is_empty());
    assert_eq!(Arc::strong_count(&reclaimer), 1);
    stop_reclaimer(reclaimer);
}

#[def_test(serial)]
fn block_irq_expired_device_is_unclaimed_and_does_not_queue_reclaiming() {
    let device = TestDevice::new(true);
    let (irq, reclaimer) = subscribe(&device, 0x4002);
    drop(device);
    assert!(!dispatch(0x4002).handled());
    // One activation reference and one IRQ closure reference, no pending entry.
    assert_eq!(Arc::strong_count(&reclaimer), 2);
    drop(irq);
    assert_eq!(Arc::strong_count(&reclaimer), 1);
    stop_reclaimer(reclaimer);
    assert!(ACTIONS.lock().is_empty());
}
