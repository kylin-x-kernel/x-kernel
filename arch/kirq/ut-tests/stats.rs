// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ receive statistics through the production dispatch entry.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::{TestResult, assert, assert_eq, def_test};

use super::dispatch_claim;
use crate::{
    IrqActionName, IrqController, IrqDesc, IrqEvent, IrqRef, IrqRegisterOptions, IrqStatsClass,
    IrqStatsError, IrqTrigger, PendingIrq, lookup_irq_line, tests_irq::SyntheticLine,
};

static FLOW: crate::FasteoiChip = crate::FasteoiChip {
    resend: crate::FasteoiResend::Software,
    mask: |_| {},
    unmask: |_| {},
};

fn mapped_line(virq: usize) -> SyntheticLine {
    let line = SyntheticLine::mapped(IrqDesc::from_virq(virq));
    line.set_flow_for_tests(crate::IrqFlow::Fasteoi(&FLOW));
    line
}

fn dispatch(line: &SyntheticLine) -> usize {
    let mut completions = 0;
    let resolved = dispatch_claim(IrqRef::Virq(line.virq()), || completions += 1);
    core::assert_eq!(completions, 1);
    resolved.expect("current line must resolve")
}

fn snapshot(line: &SyntheticLine) -> crate::IrqStatsSnapshot {
    lookup_irq_line(line.virq())
        .expect("lookup")
        .expect("current handle")
        .stats_snapshot()
        .expect("stats snapshot")
}

fn assert_counts_only_on(stats: &crate::IrqStatsSnapshot, cpu: usize, value: u64) {
    for (slot, count) in stats.counts().iter().enumerate() {
        core::assert_eq!(*count, if slot == cpu { value } else { 0 });
    }
}

#[def_test(serial)]
fn hardware_claim_counts_once_for_shared_actions() -> TestResult {
    let line = mapped_line(0x7301);
    let handler: crate::platform::Handler = Arc::new(|_| IrqEvent::HANDLED);
    assert!(
        line.install_shared_action_for_tests(handler.clone())
            .is_some()
    );
    assert!(line.install_shared_action_for_tests(handler).is_some());

    let cpu = {
        let _pin = kspin::NoPreempt::new();
        let cpu = kiface_cpu_api::CurrentCpuIf::current_cpu().as_usize();
        assert_eq!(dispatch(&line), line.virq());
        cpu
    };
    let stats = snapshot(&line);
    assert_eq!(stats.class(), IrqStatsClass::Countable);
    assert!(stats.has_actions());
    assert_counts_only_on(&stats, cpu, 1);
    assert!(stats.action_names().is_empty());
    TestResult::Ok
}

#[def_test(serial)]
fn no_action_receive_and_last_action_keep_counts() -> TestResult {
    let line = mapped_line(0x7302);
    let handler: crate::platform::Handler = Arc::new(|_| IrqEvent::HANDLED);
    let token = line
        .install_shared_action_for_tests(handler)
        .expect("shared action");
    let cpu = {
        let _pin = kspin::NoPreempt::new();
        let cpu = kiface_cpu_api::CurrentCpuIf::current_cpu().as_usize();
        assert_eq!(dispatch(&line), line.virq());
        assert!(line.withdraw_shared_action_for_tests(token));
        assert_eq!(dispatch(&line), line.virq());
        cpu
    };
    let stats = snapshot(&line);
    assert!(!stats.has_actions());
    assert_counts_only_on(&stats, cpu, 2);
    assert!(stats.action_names().is_empty());
    TestResult::Ok
}

#[def_test(serial)]
fn named_shared_registration_reaches_snapshot() -> TestResult {
    let line = mapped_line(0x730b);
    let handler: crate::platform::Handler = Arc::new(|_| IrqEvent::HANDLED);
    let token = crate::try_register_shared_named(
        line.virq(),
        handler,
        IrqActionName::new("serial").expect("valid name"),
    )
    .expect("named shared registration");
    assert!(token.is_some());
    let stats = snapshot(&line);
    assert!(stats.has_actions());
    assert_eq!(stats.action_names(), &["serial"]);
    TestResult::Ok
}

#[def_test(serial)]
fn named_regular_registration_reaches_snapshot() -> TestResult {
    let line = mapped_line(0x730f);
    let handler: crate::platform::Handler = Arc::new(|_| IrqEvent::HANDLED);
    assert!(
        crate::try_register_with_options(
            line.virq(),
            handler,
            IrqRegisterOptions::auto_enable()
                .with_name(IrqActionName::new("regular-name").expect("valid name")),
        )
        .expect("named regular registration")
    );
    let stats = snapshot(&line);
    assert!(stats.has_actions());
    assert_eq!(stats.action_names(), &["regular-name"]);
    TestResult::Ok
}

#[def_test(serial)]
fn unnamed_action_presence_is_not_empty_names() -> TestResult {
    let line = mapped_line(0x7303);
    let handler: crate::platform::Handler = Arc::new(|_| IrqEvent::HANDLED);
    assert!(line.install_shared_action_for_tests(handler).is_some());
    let stats = snapshot(&line);
    assert!(stats.has_actions());
    assert!(stats.action_names().is_empty());
    TestResult::Ok
}

#[def_test(serial)]
fn named_shared_actions_are_exposed_and_validated() -> TestResult {
    let line = mapped_line(0x7304);
    let handler: crate::platform::Handler = Arc::new(|_| IrqEvent::HANDLED);
    assert!(
        line.install_shared_named_action_for_tests(handler.clone(), "serial")
            .is_some()
    );
    assert!(
        line.install_shared_named_action_for_tests(handler, "console")
            .is_some()
    );
    let stats = snapshot(&line);
    assert_eq!(stats.action_names(), &["serial", "console"]);

    assert!(IrqActionName::new("serial").is_ok());
    assert!(IrqActionName::new("two words").is_ok());
    assert!(IrqActionName::new("").is_err());
    assert!(IrqActionName::new("   ").is_err());
    assert!(IrqActionName::new("bad,name").is_err());
    assert!(IrqActionName::new("bad\nname").is_err());
    assert!(
        IrqActionName::new("1234567890123456789012345678901234567890123456789012345678901234")
            .is_err()
    );
    TestResult::Ok
}

#[def_test(serial)]
fn empty_flow_and_nmi_receive_no_ordinary_counts() -> TestResult {
    let no_flow = SyntheticLine::mapped(IrqDesc::from_virq(0x7305));
    no_flow.clear_flow_for_tests();
    assert_eq!(dispatch(&no_flow), no_flow.virq());
    let stats = snapshot(&no_flow);
    assert_eq!(stats.class(), IrqStatsClass::NoFlow);
    assert_eq!(stats.counts().iter().sum::<u64>(), 0);

    let nmi = SyntheticLine::mapped(IrqDesc::from_virq(0x7306));
    nmi.set_flow_for_tests(crate::IrqFlow::PerCpu);
    nmi.install_nmi_action_for_tests();
    assert_eq!(dispatch(&nmi), nmi.virq());
    let stats = snapshot(&nmi);
    assert_eq!(stats.class(), IrqStatsClass::Nmi);
    assert_eq!(stats.counts().iter().sum::<u64>(), 0);
    TestResult::Ok
}

#[def_test(serial)]
fn simulated_entry_does_not_add_receive_count() -> TestResult {
    let line = mapped_line(0x730a);
    let handler: crate::platform::Handler = Arc::new(|_| IrqEvent::HANDLED);
    assert!(line.install_shared_action_for_tests(handler).is_some());
    dispatch(&line);
    let before = snapshot(&line);
    assert_eq!(before.counts().iter().sum::<u64>(), 1);

    crate::runtime::entry::dispatch_simulated_irq(PendingIrq::simulated(IrqRef::Virq(line.virq())));
    let after = snapshot(&line);
    assert_eq!(after.counts(), before.counts());
    TestResult::Ok
}

#[def_test(serial)]
fn retired_handle_cannot_read_successor_counts() -> TestResult {
    let line = mapped_line(0x7307);
    let cpu = kiface_cpu_api::CurrentCpuIf::current_cpu().as_usize();
    line.set_stats_count_for_tests(cpu, 9);
    let handle = lookup_irq_line(line.virq())
        .expect("lookup")
        .expect("handle");
    line.retire_for_tests();
    assert_eq!(
        handle.stats_snapshot().unwrap_err(),
        IrqStatsError::UnknownIrq
    );

    let successor = mapped_line(line.virq());
    let stats = snapshot(&successor);
    assert_eq!(stats.virq(), line.virq());
    assert_eq!(stats.counts().iter().sum::<u64>(), 0);
    assert_eq!(
        handle.stats_snapshot().unwrap_err(),
        IrqStatsError::UnknownIrq
    );
    TestResult::Ok
}

#[def_test(serial)]
fn snapshot_uses_current_cpu_counts_and_stable_description() -> TestResult {
    let line = mapped_line(0x7308);
    for cpu in 0..kbuild_config::NR_CPUS {
        line.set_stats_count_for_tests(cpu, cpu as u64 + 1);
    }
    let stats = snapshot(&line);
    assert_eq!(stats.virq(), line.virq());
    assert_eq!(stats.hwirq(), line.virq());
    assert_eq!(stats.trigger(), IrqTrigger::Unknown(0));
    assert_eq!(stats.controller(), IrqController::Unknown);
    assert_eq!(stats.domain(), None);
    for cpu in 0..kbuild_config::NR_CPUS {
        assert_eq!(stats.counts()[cpu], cpu as u64 + 1);
    }
    assert_eq!(stats.counts().len(), kbuild_config::NR_CPUS);
    assert_eq!(
        stats.counts().iter().sum::<u64>(),
        (1..=kbuild_config::NR_CPUS as u64).sum()
    );
    TestResult::Ok
}

#[def_test(serial)]
fn replay_runs_handler_without_adding_receive_count() -> TestResult {
    let line = mapped_line(0x730c);
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let handler: crate::platform::Handler = Arc::new(move |_| {
        observed.fetch_add(1, Ordering::Relaxed);
        IrqEvent::HANDLED
    });
    let token = crate::try_register_shared_named(
        line.virq(),
        handler,
        IrqActionName::new("replay-test").expect("valid name"),
    )
    .expect("registration");
    assert!(token.is_some());
    crate::try_enable(line.virq(), true).expect("enable replay line");

    dispatch(&line);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    let before = snapshot(&line);
    assert_eq!(before.counts().iter().sum::<u64>(), 1);

    {
        let _pin = kspin::NoPreempt::new();
        line.replay_for_tests();
    }
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    let after = snapshot(&line);
    assert_eq!(after.counts(), before.counts());
    TestResult::Ok
}

#[def_test(serial)]
fn receive_counter_wraps() -> TestResult {
    let line = mapped_line(0x730d);
    let cpu = {
        let _pin = kspin::NoPreempt::new();
        let cpu = kiface_cpu_api::CurrentCpuIf::current_cpu().as_usize();
        line.set_stats_count_for_tests(cpu, u64::MAX);
        dispatch(&line);
        cpu
    };
    let stats = snapshot(&line);
    assert_eq!(stats.counts()[cpu], 0);
    TestResult::Ok
}

#[def_test(serial)]
fn capacity_growth_retries() -> TestResult {
    let line = mapped_line(0x730e);
    let handler: crate::platform::Handler = Arc::new(|_| IrqEvent::HANDLED);
    let handle = lookup_irq_line(line.virq())
        .expect("lookup")
        .expect("handle");

    let mut added = 0;
    let result = handle.stats_snapshot_with_hook(|| {
        if added < 2 {
            let name = if added == 0 { "growth-a" } else { "growth-b" };
            core::assert!(
                line.install_shared_named_action_for_tests(handler.clone(), name)
                    .is_some()
            );
            added += 1;
        }
    });
    let stats = result.expect("growth retries should succeed");
    assert_eq!(stats.action_names().len(), 2);
    assert_eq!(added, 2);
    TestResult::Ok
}

#[def_test(serial)]
fn classify_countable_only_for_resolved_ordinary_flow() -> TestResult {
    let line = mapped_line(0x7309);
    let stats = snapshot(&line);
    assert_eq!(stats.class(), IrqStatsClass::Countable);
    assert_eq!(IrqStatsClass::Countable, stats.class());
    TestResult::Ok
}
