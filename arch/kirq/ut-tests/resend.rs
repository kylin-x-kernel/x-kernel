// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kspin::SpinNoIrq;
use intrusive_collections::LinkedList;
use unittest::def_test;

use super::{ReplayAdapter, ReplayQueue, schedule, schedule_on};
use crate::{FasteoiChip, IrqDesc, IrqEvent, IrqFlow, action::IrqAction,
    runtime::{dispatch, state::{IrqLine, IrqStateDesc, IRQ_STATE}}};

static CHIP: FasteoiChip = FasteoiChip {
    mask: |_| {},
    unmask: |_| {},
    resend: crate::FasteoiResend::Software,
};

fn fixture(virq: usize) -> (Arc<IrqLine>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut entry = IrqStateDesc::new(IrqDesc::from_virq(virq));
    entry.flow = IrqFlow::Fasteoi(&CHIP);
    let count = calls.clone();
    assert!(entry.install_regular_action(IrqAction::regular(Arc::new(move |_| {
        assert!(!karch::local_irq_enabled());
        count.fetch_add(1, Ordering::Relaxed);
        IrqEvent::HANDLED
    }))));
    let plan = entry.prepare_auto_enable();
    entry.commit_platform_plan(plan);
    entry.is_pending = true;
    (Arc::new(IrqLine::new(entry)), calls)
}

fn queue() -> SpinNoIrq<ReplayQueue> {
    SpinNoIrq::new(ReplayQueue { pending: LinkedList::new(ReplayAdapter::new()), notify: None })
}

#[def_test(serial)]
fn serving_softirq_replay_scopes_action_reads_and_drains_admission() {
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let mut entry = IrqStateDesc::new(IrqDesc::from_virq(0x5f4e));
    entry.flow = IrqFlow::Fasteoi(&CHIP);
    assert!(entry.install_regular_action(IrqAction::regular(Arc::new(move |_| {
        assert!(crate::context::is_softirq_context());
        assert!(krcu::is_reading());
        assert!(!karch::local_irq_enabled());
        observed.fetch_add(1, Ordering::Relaxed);
        IrqEvent::HANDLED
    }))));
    let plan = entry.prepare_auto_enable();
    entry.commit_platform_plan(plan);
    entry.is_pending = true;
    let line = Arc::new(IrqLine::new(entry));
    let queue = queue();
    schedule_on(&queue, &line);
    let completion = line.lock().in_flight_zero_completion();
    assert!(!completion.is_completed());
    assert!(!krcu::is_reading());
    {
        let _pin = kspin::NoPreemptIrqSave::new();
        let _softirq = crate::context::SoftIrqContextGuard::enter();
        super::run_on(&queue);
        assert!(crate::context::is_softirq_context());
        assert!(!krcu::is_reading());
        assert_eq!(line.lock().in_flight(), 0);
        assert!(completion.is_completed());
    }
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(!crate::context::is_softirq_context());
    assert!(!krcu::is_reading());
    assert!(queue.lock().pending.is_empty());
    assert!(!line.lock().is_running);
    assert!(!line.lock().is_replaying);
}

static HARDWARE_REQUESTS: AtomicUsize = AtomicUsize::new(0);
static HARDWARE_ACCEPTS: AtomicBool = AtomicBool::new(true);
static LEVEL_CHIP: FasteoiChip = FasteoiChip {
    mask: |_| {},
    unmask: |_| {},
    resend: crate::FasteoiResend::Level,
};

#[def_test]
fn validated_hardware_level_needs_no_replay_for_unspecified_firmware_trigger() {
    let (line, calls) = fixture(0x5f4b);
    line.lock().flow = IrqFlow::Fasteoi(&LEVEL_CHIP);
    let queue = queue();
    {
        let mut entry = line.lock();
        let plan = entry.prepare_disable_irq_nosync();
        entry.commit_platform_plan(plan);
    }
    schedule_on(&queue, &line);
    assert!(line.lock().is_pending);
    {
        let mut entry = line.lock();
        let plan = entry.prepare_enable_irq();
        entry.commit_platform_plan(plan);
    }
    schedule_on(&queue, &line);
    assert!(!line.lock().is_pending);
    assert_no_software_service(&line, &queue, &calls);
}
static HARDWARE_CHIP: FasteoiChip = FasteoiChip {
    mask: |_| {},
    unmask: |_| {},
    resend: crate::FasteoiResend::HardwareOnly(|desc| {
        assert!(!karch::local_irq_enabled());
        assert_eq!(desc.hwirq, 0x5f4a);
        HARDWARE_REQUESTS.fetch_add(1, Ordering::Relaxed);
        HARDWARE_ACCEPTS.load(Ordering::Relaxed)
    }),
};

fn hardware_fixture() -> (Arc<IrqLine>, Arc<AtomicUsize>) {
    HARDWARE_REQUESTS.store(0, Ordering::Relaxed);
    HARDWARE_ACCEPTS.store(true, Ordering::Relaxed);
    let (line, calls) = fixture(0x5f4a);
    line.lock().flow = IrqFlow::Fasteoi(&HARDWARE_CHIP);
    (line, calls)
}

fn assert_no_software_service(
    line: &IrqLine,
    queue: &SpinNoIrq<ReplayQueue>,
    calls: &AtomicUsize,
) {
    assert!(queue.lock().pending.is_empty());
    let entry = line.lock();
    assert!(!entry.is_replay_queued);
    assert_eq!(entry.in_flight(), 0);
    assert!(entry.in_flight_zero_completion().is_completed());
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[def_test(serial)]
fn hardware_success_consumes_pending_and_deduplicates_requests() {
    let (line, calls) = hardware_fixture();
    let queue = queue();
    schedule_on(&queue, &line);
    schedule_on(&queue, &line);
    assert_eq!(HARDWARE_REQUESTS.load(Ordering::Relaxed), 1);
    assert!(!line.lock().is_pending);
    assert_no_software_service(&line, &queue, &calls);
    line.lock().is_pending = true;
    schedule_on(&queue, &line);
    assert_eq!(HARDWARE_REQUESTS.load(Ordering::Relaxed), 2);
    assert!(!line.lock().is_pending);
    assert_no_software_service(&line, &queue, &calls);
}

#[def_test(serial)]
fn hardware_failure_retains_pending_without_software_fallback() {
    let (line, calls) = hardware_fixture();
    let queue = queue();
    HARDWARE_ACCEPTS.store(false, Ordering::Relaxed);
    schedule_on(&queue, &line);
    assert_eq!(HARDWARE_REQUESTS.load(Ordering::Relaxed), 1);
    assert!(line.lock().is_pending);
    assert_no_software_service(&line, &queue, &calls);
    // Only a subsequent explicit progress attempt retries the failed request.
    HARDWARE_ACCEPTS.store(true, Ordering::Relaxed);
    schedule_on(&queue, &line);
    schedule_on(&queue, &line);
    assert_eq!(HARDWARE_REQUESTS.load(Ordering::Relaxed), 2);
    assert!(!line.lock().is_pending);
    assert_no_software_service(&line, &queue, &calls);
}

#[def_test(serial)]
fn hardware_resend_waits_for_final_enable_and_running_owner() {
    let (line, calls) = hardware_fixture();
    let queue = queue();
    {
        let mut entry = line.lock();
        let plan = entry.prepare_disable_irq_nosync();
        entry.commit_platform_plan(plan);
        let plan = entry.prepare_disable_irq_nosync();
        entry.commit_platform_plan(plan);
    }
    schedule_on(&queue, &line);
    {
        let mut entry = line.lock();
        let plan = entry.prepare_enable_irq();
        entry.commit_platform_plan(plan);
    }
    schedule_on(&queue, &line);
    {
        let mut entry = line.lock();
        let plan = entry.prepare_enable_irq();
        entry.commit_platform_plan(plan);
        entry.is_running = true;
    }
    schedule_on(&queue, &line);
    assert_eq!(HARDWARE_REQUESTS.load(Ordering::Relaxed), 0);
    assert!(line.lock().is_pending);
    assert_no_software_service(&line, &queue, &calls);
    line.lock().is_running = false;
    schedule_on(&queue, &line);
    assert_eq!(HARDWARE_REQUESTS.load(Ordering::Relaxed), 1);
    assert!(!line.lock().is_pending);
    assert_no_software_service(&line, &queue, &calls);
}

#[def_test(serial)]
fn discarded_enable_plan_does_not_release_pending_resend() {
    for chip in [&CHIP, &HARDWARE_CHIP] {
        let (line, calls) = hardware_fixture();
        let queue = queue();
        {
            let mut entry = line.lock();
            entry.flow = IrqFlow::Fasteoi(chip);
            let disable = entry.prepare_disable_irq_nosync();
            entry.commit_platform_plan(disable);
            let abandoned = entry.prepare_enable_irq();
            assert_eq!(abandoned.enable, Some(true));
            assert!(abandoned.flow_unmask.is_some());
            assert!(!entry.can_run_actions());
        }
        // Preparation alone must not make a pending obligation eligible.
        schedule_on(&queue, &line);
        assert!(line.lock().is_pending);
        assert_eq!(HARDWARE_REQUESTS.load(Ordering::Relaxed), 0);
        assert_no_software_service(&line, &queue, &calls);

        {
            let mut entry = line.lock();
            let retry = entry.prepare_enable_irq();
            assert_eq!(retry.enable, Some(true));
            // The fixture models successful platform application; it performs
            // no real controller access or hardware delivery.
            entry.commit_platform_plan(retry);
        }
        schedule_on(&queue, &line);
        match chip.resend {
            crate::FasteoiResend::Level => panic!("fixture uses software or hardware resend"),
            crate::FasteoiResend::Software => {
                assert_eq!(line.lock().in_flight(), 1);
                let queued = queue.lock().pending.pop_front().unwrap();
                dispatch::replay(queued);
                assert_eq!(calls.load(Ordering::Relaxed), 1);
                assert_eq!(HARDWARE_REQUESTS.load(Ordering::Relaxed), 0);
            }
            crate::FasteoiResend::HardwareOnly(_) => {
                assert_eq!(HARDWARE_REQUESTS.load(Ordering::Relaxed), 1);
                assert_no_software_service(&line, &queue, &calls);
            }
        }
        assert!(!line.lock().is_pending);
        assert_eq!(line.lock().in_flight(), 0);
        assert!(line.lock().in_flight_zero_completion().is_completed());
    }
}

#[def_test(serial)]
fn explicit_level_never_requests_hardware_resend() {
    for trigger in [crate::IrqTrigger::LevelHigh, crate::IrqTrigger::LevelLow] {
        let (line, calls) = hardware_fixture();
        let queue = queue();
        {
            let mut entry = line.lock();
            entry.desc.trigger = trigger;
            let plan = entry.prepare_disable_irq_nosync();
            entry.commit_platform_plan(plan);
        }
        schedule_on(&queue, &line);
        assert!(line.lock().is_pending);
        {
            let mut entry = line.lock();
            let plan = entry.prepare_enable_irq();
            entry.commit_platform_plan(plan);
        }
        schedule_on(&queue, &line);
        schedule_on(&queue, &line);
        assert!(!line.lock().is_pending);
        assert_eq!(HARDWARE_REQUESTS.load(Ordering::Relaxed), 0);
        assert_no_software_service(&line, &queue, &calls);
    }
}

#[def_test(serial)]
fn repeated_requests_retain_one_service_obligation() {
    let (line, calls) = fixture(0x5f40);
    let queue = queue();
    assert_eq!(Arc::strong_count(&line), 1);
    schedule_on(&queue, &line);
    schedule_on(&queue, &line);
    assert_eq!(Arc::strong_count(&line), 2);
    assert_eq!(line.lock().in_flight(), 1);
    let queued = queue.lock().pending.pop_front().unwrap();
    assert!(Arc::ptr_eq(&queued, &line));
    assert_eq!(Arc::strong_count(&line), 2);
    assert!(queue.lock().pending.is_empty());
    dispatch::replay(queued);
    assert_eq!(Arc::strong_count(&line), 1);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(line.lock().in_flight(), 0);
    assert!(!line.lock().is_pending);
    schedule_on(&queue, &line);
    assert!(queue.lock().pending.is_empty());
}

#[def_test(serial)]
fn queued_replay_does_not_snapshot_a_withdrawn_action() {
    let (line, calls) = fixture(0x5f41);
    let queue = queue();
    schedule_on(&queue, &line);
    let completion = line.lock().in_flight_zero_completion();
    line.lock().begin_teardown();
    // No Line guard spans preparation or GP reclamation. This private queue
    // retains admission but has not borrowed an action, so it cannot block GP.
    let action = super::super::manager::take_from_line(&line, None);
    {
        let mut entry = line.lock();
        if let Some(plan) = entry.prepare_disable_if_no_actions() {
            entry.commit_platform_plan(plan);
        }
    }
    assert!(action.is_some());
    assert!(!completion.is_completed());
    let queued = queue.lock().pending.pop_front().unwrap();
    dispatch::replay(queued);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert!(completion.is_completed());
    assert!(!line.lock().is_running);
    line.lock().finish_teardown();
    // Keep task-owned handler destruction after the queued admission drains.
    drop(action);
}

#[def_test(serial)]
fn disable_after_enqueue_drains_the_old_obligation_but_retains_pending() {
    let (line, calls) = fixture(0x5f48);
    let queue = queue();
    schedule_on(&queue, &line);
    let completion = line.lock().in_flight_zero_completion();
    {
        let mut entry = line.lock();
        let plan = entry.prepare_disable_irq_nosync();
        entry.commit_platform_plan(plan);
    }
    assert!(!completion.is_completed());
    super::run_on(&queue);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert!(completion.is_completed());
    {
        let mut entry = line.lock();
        assert!(entry.is_pending);
        assert!(!entry.is_replay_queued);
        assert_eq!(entry.in_flight(), 0);
        let plan = entry.prepare_enable_irq();
        entry.commit_platform_plan(plan);
    }
    schedule_on(&queue, &line);
    assert!(!completion.is_completed());
    super::run_on(&queue);
    assert!(completion.is_completed());
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(!line.lock().is_pending);
    assert!(queue.lock().pending.is_empty());
}

#[def_test(serial)]
fn configuration_failure_drains_queued_replay_without_service_until_recovery() {
    let (line, calls) = fixture(0x5f4b);
    let queue = queue();
    schedule_on(&queue, &line);
    let completion = line.lock().in_flight_zero_completion();
    {
        let mut entry = line.lock();
        // A controller can fail even when descriptor generation is unchanged.
        entry.mark_configuration_failed();
        assert_eq!(entry.configured_generation_for_tests(), Some(entry.generation_for_tests()));
        assert!(entry.is_enabled_for_tests());
        assert_eq!(entry.disable_depth_for_tests(), 0);
        assert!(!entry.can_run_actions());
        assert!(entry.needs_configuration_restore());
    }
    assert!(!completion.is_completed());
    super::run_on(&queue);
    assert!(completion.is_completed());
    assert!(line.lock().is_pending);
    assert!(!line.lock().is_running);
    schedule_on(&queue, &line);
    assert_no_software_service(&line, &queue, &calls);
    {
        let mut entry = line.lock();
        let retry = entry.prepare_enable_irq();
        assert!(retry.configure);
        assert_eq!(retry.enable, Some(true));
        assert!(retry.flow_unmask.is_some());
        assert!(!entry.can_run_actions());
        entry.commit_platform_plan(retry);
        assert!(entry.can_run_actions());
        assert!(!entry.needs_configuration_restore());
    }
    schedule_on(&queue, &line);
    assert!(!completion.is_completed());
    super::run_on(&queue);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(completion.is_completed());
    assert!(!line.lock().is_pending);
    assert_eq!(line.lock().in_flight(), 0);
    assert!(queue.lock().pending.is_empty());
}

#[def_test(serial)]
fn configuration_failure_suppresses_hardware_resend_until_configuration_commit() {
    let (line, calls) = hardware_fixture();
    let queue = queue();
    {
        let mut entry = line.lock();
        entry.mark_configuration_failed();
        let disable = entry.prepare_disable_irq_nosync();
        entry.commit_platform_plan(disable);
        assert!(!entry.needs_configuration_restore());
        let nested_disable = entry.prepare_disable_irq_nosync();
        entry.commit_platform_plan(nested_disable);
        let partial_enable = entry.prepare_enable_irq();
        assert!(!partial_enable.configure);
        entry.commit_platform_plan(partial_enable);
        assert!(entry.prepare_reconfigure_if_stale().configure);
        assert!(!entry.can_run_actions());
    }
    schedule_on(&queue, &line);
    assert_eq!(HARDWARE_REQUESTS.load(Ordering::Relaxed), 0);
    assert!(line.lock().is_pending);
    assert_no_software_service(&line, &queue, &calls);
    {
        let mut entry = line.lock();
        let retry = entry.prepare_enable_irq();
        assert!(retry.configure);
        assert_eq!(retry.enable, Some(true));
        entry.commit_platform_plan(retry);
        assert!(entry.can_run_actions());
    }
    schedule_on(&queue, &line);
    assert_eq!(HARDWARE_REQUESTS.load(Ordering::Relaxed), 1);
    assert!(!line.lock().is_pending);
    assert_no_software_service(&line, &queue, &calls);
}

#[def_test(serial)]
fn metadata_merge_alone_keeps_pending_replay_eligible() {
    let (line, calls) = fixture(0x5f4c);
    let queue = queue();
    {
        let mut entry = line.lock();
        let richer = entry.desc.with_flags(crate::IrqFlags::WAKEUP_SOURCE);
        entry.update_desc(richer);
        assert_ne!(entry.configured_generation_for_tests(), Some(entry.generation_for_tests()));
        assert!(entry.prepare_reconfigure_if_stale().configure);
        assert!(entry.can_run_actions());
        assert!(entry.needs_replay());
        assert!(!entry.needs_configuration_restore());
    }
    schedule_on(&queue, &line);
    super::run_on(&queue);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(!line.lock().is_pending);
    assert_eq!(line.lock().in_flight(), 0);
    assert!(queue.lock().pending.is_empty());
}

#[def_test(serial)]
fn replay_handler_disable_preserves_pending_until_explicit_enable() {
    let calls = Arc::new(AtomicUsize::new(0));
    let line = Arc::new_cyclic(|weak: &alloc::sync::Weak<IrqLine>| {
        let mut entry = IrqStateDesc::new(IrqDesc::from_virq(0x5f49));
        entry.flow = IrqFlow::Fasteoi(&CHIP);
        let weak = weak.clone();
        let calls = calls.clone();
        assert!(entry.install_regular_action(IrqAction::regular(Arc::new(move |_| {
            let line = weak.upgrade().unwrap();
            let mut entry = line.lock();
            assert!(entry.is_running);
            assert_eq!(entry.in_flight(), 1);
            assert!(!entry.in_flight_zero_completion().is_completed());
            if calls.fetch_add(1, Ordering::Relaxed) == 0 {
                let disable = entry.prepare_disable_irq_nosync();
                entry.commit_platform_plan(disable);
                entry.is_pending = true;
            }
            IrqEvent::HANDLED
        }))));
        let plan = entry.prepare_auto_enable();
        entry.commit_platform_plan(plan);
        entry.is_pending = true;
        IrqLine::new(entry)
    });
    let queue = queue();
    schedule_on(&queue, &line);
    super::run_on(&queue);
    {
        let mut entry = line.lock();
        assert!(entry.is_pending);
        assert!(!entry.is_running);
        assert!(!entry.is_replay_queued);
        assert_eq!(entry.in_flight(), 0);
        assert!(entry.in_flight_zero_completion().is_completed());
        assert_eq!(entry.disable_depth_for_tests(), 1);
        let enable = entry.prepare_enable_irq();
        entry.commit_platform_plan(enable);
    }
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    schedule_on(&queue, &line);
    super::run_on(&queue);
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert_eq!(line.lock().in_flight(), 0);
    assert!(!line.lock().is_pending);
    assert!(queue.lock().pending.is_empty());
}

#[def_test(serial)]
fn real_claim_during_replay_preserves_successor_service() {
    let virq = 0x5f4d;
    let calls = Arc::new(AtomicUsize::new(0));
    let completions = Arc::new(AtomicUsize::new(0));
    let line = Arc::new_cyclic(|weak: &alloc::sync::Weak<IrqLine>| {
        let mut entry = IrqStateDesc::new(IrqDesc::from_virq(virq));
        entry.flow = IrqFlow::Fasteoi(&CHIP);
        let weak = weak.clone();
        let calls = calls.clone();
        let completions = completions.clone();
        assert!(entry.install_regular_action(IrqAction::regular(Arc::new(move |_| {
            if calls.fetch_add(1, Ordering::Relaxed) == 0 {
                // Inject the cross-CPU ordering deterministically after the
                // replay handler's device work, without depending on timing.
                assert_eq!(dispatch::dispatch_claim(crate::IrqRef::Virq(virq), || {
                    completions.fetch_add(1, Ordering::Relaxed);
                }), Some(virq));
                let line = weak.upgrade().unwrap();
                let mut entry = line.lock();
                assert!(entry.is_pending);
                assert!(entry.is_running);
                assert_eq!(entry.in_flight(), 1);
                // Keep the successor on this fixture's private queue rather
                // than letting the global BH executor race the assertions.
                let disable = entry.prepare_disable_irq_nosync();
                entry.commit_platform_plan(disable);
            }
            IrqEvent::HANDLED
        }))));
        let enable = entry.prepare_auto_enable();
        entry.commit_platform_plan(enable);
        entry.is_pending = true;
        IrqLine::new(entry)
    });
    assert!(IRQ_STATE.lock().descs.insert(virq, line.clone()).is_none());
    let queue = queue();
    schedule_on(&queue, &line);
    super::run_on(&queue);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(completions.load(Ordering::Relaxed), 1);
    {
        let mut entry = line.lock();
        assert!(entry.is_pending);
        assert!(!entry.is_running);
        assert_eq!(entry.in_flight(), 0);
        let enable = entry.prepare_enable_irq();
        entry.commit_platform_plan(enable);
    }
    schedule_on(&queue, &line);
    super::run_on(&queue);
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert_eq!(completions.load(Ordering::Relaxed), 1);
    assert!(!line.lock().is_pending);
    assert_eq!(line.lock().in_flight(), 0);
    assert!(line.lock().in_flight_zero_completion().is_completed());
    let removed = IRQ_STATE.lock().descs.remove(&virq).unwrap();
    assert!(Arc::ptr_eq(&removed, &line));
}

#[def_test(serial)]
fn hardware_service_consumes_pending_before_queued_replay() {
    let (line, calls) = fixture(0x5f42);
    let queue = queue();
    schedule_on(&queue, &line);
    line.lock().is_pending = false;
    let queued = queue.lock().pending.pop_front().unwrap();
    dispatch::replay(queued);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert_eq!(line.lock().in_flight(), 0);
}

#[def_test(serial)]
fn explicit_level_waits_for_final_enable_then_relies_on_hardware() {
    let (line, calls) = fixture(0x5f47);
    let queue = queue();
    {
        let mut entry = line.lock();
        entry.desc.trigger = crate::IrqTrigger::LevelHigh;
        let plan = entry.prepare_disable_irq_nosync();
        entry.commit_platform_plan(plan);
        let plan = entry.prepare_disable_irq_nosync();
        entry.commit_platform_plan(plan);
        let plan = entry.prepare_enable_irq();
        entry.commit_platform_plan(plan);
    }
    schedule_on(&queue, &line);
    assert!(line.lock().is_pending);
    assert!(queue.lock().pending.is_empty());
    {
        let mut entry = line.lock();
        let plan = entry.prepare_enable_irq();
        entry.commit_platform_plan(plan);
    }
    schedule_on(&queue, &line);
    assert!(!line.lock().is_pending);
    assert!(queue.lock().pending.is_empty());
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[def_test(serial)]
fn replay_never_redirects_to_a_replacement_line() {
    let virq = 0x5f43;
    let (original, calls) = fixture(virq);
    let (replacement, replacement_calls) = fixture(virq);
    let queue = queue();
    schedule_on(&queue, &original);
    // Deliberately bypass retirement to isolate original-identity retention.
    assert!(IRQ_STATE.lock().descs.insert(virq, replacement.clone()).is_none());
    let queued = queue.lock().pending.pop_front().unwrap();
    dispatch::replay(queued);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(replacement_calls.load(Ordering::Relaxed), 0);
    assert_eq!(replacement.lock().in_flight(), 0);
    let stored = IRQ_STATE.lock().descs.remove(&virq).unwrap();
    assert!(Arc::ptr_eq(&stored, &replacement));
}

#[def_test(serial)]
fn running_owner_defers_replay_without_losing_pending() {
    let (line, calls) = fixture(0x5f44);
    let queue = queue();
    schedule_on(&queue, &line);
    line.lock().is_running = true;
    let queued = queue.lock().pending.pop_front().unwrap();
    dispatch::replay(queued);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert!(line.lock().is_running);
    assert!(line.lock().is_pending);
    assert_eq!(line.lock().in_flight(), 0);
    line.lock().is_running = false;
    schedule_on(&queue, &line);
    let queued = queue.lock().pending.pop_front().unwrap();
    dispatch::replay(queued);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[def_test(serial)]
fn permanent_bh_executor_drains_original_line() {
    let (line, calls) = fixture(0x5f45);
    let completion = line.lock().in_flight_zero_completion();
    schedule(&line);
    kiface_task_wait_api::wait_for_completion(&completion).unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(line.lock().in_flight(), 0);
}

#[def_test(serial)]
fn permanent_bh_executor_drains_a_multi_batch_burst() {
    let fixtures: alloc::vec::Vec<_> = (0..=super::BATCH_LIMIT)
        .map(|index| fixture(0x6000 + index))
        .collect();
    {
        // Avoid local BH execution while publishing this burst. A predecessor
        // invocation on another CPU may still consume work concurrently.
        let _guard = kspin::NoPreemptIrqSave::new();
        for (line, _) in &fixtures {
            schedule(line);
        }
    }
    for (line, calls) in fixtures {
        let completion = line.lock().in_flight_zero_completion();
        kiface_task_wait_api::wait_for_completion(&completion).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(line.lock().in_flight(), 0);
    }
}

#[def_test(serial)]
fn batch_limit_leaves_one_line_and_notifies_for_successor() {
    static NOTIFICATIONS: AtomicUsize = AtomicUsize::new(0);
    let fixtures: alloc::vec::Vec<_> = (0..=super::BATCH_LIMIT)
        .map(|index| fixture(0x6100 + index))
        .collect();
    let queue = queue();
    for (line, _) in &fixtures {
        schedule_on(&queue, line);
    }
    NOTIFICATIONS.store(0, Ordering::Relaxed);
    queue.lock().notify = Some(|| { NOTIFICATIONS.fetch_add(1, Ordering::Relaxed); });
    super::run_on(&queue);
    assert_eq!(NOTIFICATIONS.load(Ordering::Relaxed), 1);
    assert!(!queue.lock().pending.is_empty());
    assert_eq!(fixtures.last().unwrap().1.load(Ordering::Relaxed), 0);
    super::run_on(&queue);
    assert!(queue.lock().pending.is_empty());
    for (line, calls) in fixtures {
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(line.lock().in_flight(), 0);
    }
}
