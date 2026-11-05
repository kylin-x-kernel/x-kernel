// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use unittest::{assert, assert_eq, def_test};

use super::{IrqDesc, IrqFlags, IrqLine, IrqState, IrqStateDesc, try_resolve_and_publish_locked};
use crate::{IrqDescError, IrqSpec};

#[def_test]
fn nmi_close_keeps_enabled_cpus_until_each_local_disable_completes() {
    let desc = IrqDesc::from_virq(0x5fb2);
    let mut entry = IrqStateDesc::new(desc);
    entry.flow = crate::IrqFlow::PerCpu;
    // Exercise every configured slot without depending on scheduler migration
    // or firmware-present CPUs. LocalControl's explicit CPU is a private fixture.
    for (cpu, slot) in entry.local.iter_mut().enumerate() {
        let enabled = slot.enable(desc, 1, cpu, true, false);
        slot.commit(enabled, 1);
    }
    entry.begin_nmi_close();
    for cpu in 0..entry.local.len() {
        assert!(entry.has_enabled_local_cpu());
        let disabled = entry.local[cpu].disable(desc, cpu);
        // Planning is not completion and must leave the CPU obligation visible.
        assert!(entry.local[cpu].is_enabled);
        entry.local[cpu].commit(disabled, 1);
        assert!(!entry.local[cpu].is_enabled);
        assert!(entry.is_nmi_closing());
    }
    assert!(!entry.has_enabled_local_cpu());
    assert!(entry.is_nmi_closing());
}

#[def_test]
fn existing_mapping_needs_no_new_index_capacity() {
    let desc = IrqDesc::from_hwirq(17)
        .with_domain(crate::domain::TEST_DOMAIN)
        .with_virq(0x5f90);
    let line = Arc::new(IrqLine::new(IrqStateDesc::new(desc)));
    let mut state = IrqState::new();
    state.try_resolve_desc(desc, Some(&line)).unwrap();
    let mut prepared = super::PreparedPublication::new(0x5f90, None, false).unwrap();
    let resolution = state.plan_resolution(desc.into()).unwrap();
    let result = state.commit_resolution(resolution, &line, &mut line.lock(), &mut prepared);
    assert!(result.is_ok());
    assert_eq!(
        state.translated_hwirq(crate::domain::TEST_DOMAIN, 17),
        Some(0x5f90)
    );
    assert!(prepared.logical.is_none() && prepared.domain.is_none());
}

#[def_test]
fn msi_release_requires_reservation_and_consumes_it_once_after_withdrawal() {
    let virq = 0x5f52;
    let desc = IrqDesc::from_virq(virq).with_flags(IrqFlags::MSI);
    let line = Arc::new(IrqLine::new(IrqStateDesc::new(desc)));
    let withdrawal = super::PreparedWithdrawal::new(desc.hwirq).unwrap();
    line.lock().msi_withdrawal = Some(withdrawal);
    let mut state = IrqState::new();
    state.try_resolve_desc(desc, Some(&line)).unwrap();
    let mut calls = 0;

    // A flag or caller-supplied number does not prove backend ownership.
    assert!(
        super::release_unused_msi(&mut state, virq, |_, _| {
            calls += 1;
        })
        .is_none()
    );
    line.lock().msi_reservation = Some(super::MsiReservation::for_test(desc.hwirq));

    line.lock().begin_management();
    assert!(
        super::release_unused_msi(&mut state, virq, |_, _| {
            calls += 1;
        })
        .is_none()
    );
    assert_eq!(calls, 0);
    line.lock().end_management();

    // Reject a mismatched owner before any withdrawal or backend call.
    line.lock().msi_reservation = Some(super::MsiReservation::for_test(desc.hwirq + 1));
    assert!(
        super::release_unused_msi(&mut state, virq, |_, _| {
            calls += 1;
        })
        .is_none()
    );
    assert_eq!(calls, 0);
    assert!(
        state
            .descs
            .get(&virq)
            .is_some_and(|stored| Arc::ptr_eq(&stored, &line))
    );

    line.lock().msi_reservation = Some(super::MsiReservation::for_test(desc.hwirq));
    let mut released = None;
    let removed = super::release_unused_msi(&mut state, virq, |registry, reservation| {
        released = Some((reservation.hwirq(), registry.descs.contains_key(&virq)));
        calls += 1;
    })
    .unwrap();
    assert_eq!(released, Some((desc.hwirq, false)));
    assert!(Arc::ptr_eq(&removed, &line));
    assert!(line.lock().msi_reservation.is_none());
    drop(removed);
    assert!(!state.descs.contains_key(&virq));
    assert!(
        super::release_unused_msi(&mut state, virq, |_, _| {
            calls += 1;
        })
        .is_none()
    );
    assert_eq!(calls, 1);
}

#[def_test]
fn msi_release_rejects_non_msi_without_calling_backend() {
    let virq = 0x5f53;
    let desc = IrqDesc::from_virq(virq);
    let line = Arc::new(IrqLine::new(IrqStateDesc::new(desc)));
    let mut state = IrqState::new();
    state.try_resolve_desc(desc, Some(&line)).unwrap();
    let mut has_called_backend = false;
    assert!(
        super::release_unused_msi(&mut state, virq, |_, _| {
            has_called_backend = true;
        })
        .is_none()
    );
    assert!(!has_called_backend);
    assert!(
        state
            .descs
            .get(&virq)
            .is_some_and(|stored| Arc::ptr_eq(&stored, &line))
    );
}

#[def_test]
fn msi_release_rejects_action_dispatch_and_teardown_ownership() {
    let virq = 0x5f54;
    let desc = IrqDesc::from_virq(virq).with_flags(IrqFlags::MSI);
    let line = Arc::new(IrqLine::new(IrqStateDesc::new(desc)));
    let withdrawal = super::PreparedWithdrawal::new(desc.hwirq).unwrap();
    line.lock().msi_withdrawal = Some(withdrawal);
    let mut state = IrqState::new();
    state.try_resolve_desc(desc, Some(&line)).unwrap();
    line.lock().msi_reservation = Some(super::MsiReservation::for_test(desc.hwirq));
    assert!(
        line.lock()
            .install_regular_action(crate::action::IrqAction::regular(Arc::new(|_| {
                crate::IrqEvent::HANDLED
            }),))
    );
    let mut calls = 0;
    assert!(
        super::release_unused_msi(&mut state, virq, |_, _| {
            calls += 1;
        })
        .is_none()
    );
    assert_eq!(calls, 0);

    // Removing the action does not release an already-admitted dispatch.
    line.lock().begin_dispatch();
    let removed = line.lock().take_regular_action();
    assert!(removed.is_some());
    drop(removed);
    assert!(
        super::release_unused_msi(&mut state, virq, |_, _| {
            calls += 1;
        })
        .is_none()
    );
    assert_eq!(calls, 0);

    line.lock().begin_teardown();
    let wake = line.lock().finish_dispatch();
    drop(wake);
    assert!(
        super::release_unused_msi(&mut state, virq, |_, _| {
            calls += 1;
        })
        .is_none()
    );
    assert_eq!(calls, 0);
    assert!(
        state
            .descs
            .get(&virq)
            .is_some_and(|stored| Arc::ptr_eq(&stored, &line))
    );

    line.lock().finish_teardown();
    assert!(
        super::release_unused_msi(&mut state, virq, |_, _| {
            calls += 1;
        })
        .is_some()
    );
    assert_eq!(calls, 1);
    assert!(!state.descs.contains_key(&virq));
}

#[def_test]
fn enabled_local_slot_retains_empty_line_and_flow_identity() {
    let desc = IrqDesc::from_virq(0x5f83);
    let line = Arc::new(IrqLine::new(IrqStateDesc::new(desc)));
    let mut state = IrqState::new();
    state.try_resolve_desc(desc, Some(&line)).unwrap();
    {
        let mut entry = line.lock();
        entry.flow = crate::IrqFlow::PerCpu;
        let plan = entry.prepare_auto_enable();
        entry.commit_platform_plan(plan);
        assert!(entry.has_enabled_local_cpu());
        assert!(!entry.is_unused());
    }
    assert!(state.remove_if_unused(desc.hwirq).is_none());
    assert!(matches!(
        state.try_resolve_desc(desc, None),
        Err(IrqDescError::UnsupportedFlow { .. })
    ));
    {
        let mut entry = line.lock();
        let plan = entry.prepare_disable_irq_nosync();
        entry.commit_platform_plan(plan);
        assert!(!entry.has_enabled_local_cpu());
    }
    state.try_resolve_desc(desc, None).unwrap();
    assert_eq!(line.lock().flow, crate::IrqFlow::Legacy);
    assert!(state.remove_if_unused(desc.hwirq).is_some());
}

#[cfg(target_arch = "aarch64")]
#[def_test]
fn gic_spi_cannot_change_flow_while_actions_or_original_claim_remain() {
    let original = crate::gic_edge_irq_desc(32).with_virq(0x5f59);
    let private = original.with_flags(IrqFlags::PER_CPU);
    for has_action in [false, true] {
        // Real provider selection, private registry: never publish a reverse
        // mapping or call the live controller's source operations.
        let mut state = IrqState::new();
        let line = Arc::new(IrqLine::new(IrqStateDesc::new(original)));
        state.try_resolve_desc(original, Some(&line)).unwrap();
        let flow = {
            let mut entry = line.lock();
            assert!(matches!(entry.flow, crate::IrqFlow::Fasteoi(_)));
            if has_action {
                assert!(
                    entry.install_regular_action(crate::action::IrqAction::regular(Arc::new(
                        |_| crate::IrqEvent::HANDLED
                    ),))
                );
            } else {
                entry.begin_dispatch();
            }
            entry.flow
        };
        assert!(matches!(
            state.try_resolve_desc(private, None),
            Err(IrqDescError::UnsupportedFlow { hwirq: 32 })
        ));
        assert_eq!(line.lock().desc, original);
        assert_eq!(line.lock().flow, flow);
        if has_action {
            let removed = line.lock().take_regular_action();
            assert!(removed.is_some());
            drop(removed);
        } else {
            let wake = line.lock().finish_dispatch();
            drop(wake);
        }
        state.try_resolve_desc(private, None).unwrap();
        assert_eq!(line.lock().flow, crate::IrqFlow::Legacy);
        assert!(
            state
                .descs
                .get(&0x5f59)
                .is_some_and(|stored| Arc::ptr_eq(&stored, &line))
        );
    }
}

#[def_test]
fn flow_reselection_waits_for_actions_and_in_flight_admission() {
    static CHIP: crate::FasteoiChip = crate::FasteoiChip {
        mask: |_| {},
        unmask: |_| {},
        resend: crate::FasteoiResend::Software,
    };
    let desc = IrqDesc::from_virq(0x5f57);
    assert_eq!(
        crate::IntrManagerIf::select_flow(desc),
        Ok(crate::IrqFlow::Legacy)
    );
    static EDGE: crate::EdgeChip = crate::EdgeChip {
        mask: |_| {},
        unmask: |_| {},
        retrigger: |_| true,
    };
    static LEVEL: crate::LevelChip = crate::LevelChip {
        mask_ack: |_| {},
        unmask: |_| {},
    };
    for old_flow in [
        crate::IrqFlow::Fasteoi(&CHIP),
        crate::IrqFlow::Edge(&EDGE),
        crate::IrqFlow::Level(&LEVEL),
    ] {
        for has_action in [true, false] {
            let mut state = IrqState::new();
            let line = Arc::new(IrqLine::new(IrqStateDesc::new(desc)));
            state.try_resolve_desc(desc, Some(&line)).unwrap();
            let generation = {
                let mut entry = line.lock();
                // Model an old flow on a private Line. The real provider selects
                // Legacy for this directory-only identity; no chip callback runs.
                entry.flow = old_flow;
                if has_action {
                    assert!(
                        entry.install_regular_action(crate::action::IrqAction::regular(Arc::new(
                            |_| crate::IrqEvent::HANDLED
                        ),))
                    );
                } else {
                    entry.begin_dispatch();
                }
                entry.generation_for_tests()
            };
            let result = state.try_resolve_desc(desc, None);
            assert!(
                matches!(result, Err(IrqDescError::UnsupportedFlow { hwirq }) if hwirq == desc.hwirq)
            );
            {
                let entry = line.lock();
                assert_eq!(entry.flow, old_flow);
                assert_eq!(entry.desc, desc);
                assert_eq!(entry.generation_for_tests(), generation);
            }
            if has_action {
                let removed = line.lock().take_regular_action();
                assert!(removed.is_some());
                drop(removed);
            } else {
                let wake = line.lock().finish_dispatch();
                drop(wake);
            }
            state.try_resolve_desc(desc, None).unwrap();
            assert_eq!(line.lock().flow, crate::IrqFlow::Legacy);
            assert!(
                state
                    .descs
                    .get(&desc.hwirq)
                    .is_some_and(|stored| Arc::ptr_eq(&stored, &line))
            );
        }
    }
}

// The initialized RISC-V provider selects actual PLIC Fasteoi operations.
// Only a private registry is changed; no mapping snapshot or MMIO is published.
#[cfg(target_arch = "riscv64")]
#[def_test]
fn plic_metadata_enrichment_waits_for_actions_and_in_flight_admission() {
    let original = crate::plic_irq_desc(1).with_virq(0x5f58);
    let enriched = IrqDesc {
        trigger: crate::IrqTrigger::LevelHigh,
        ..original
    };
    for has_action in [true, false] {
        let mut state = IrqState::new();
        let line = Arc::new(IrqLine::new(IrqStateDesc::new(original)));
        state.try_resolve_desc(original, Some(&line)).unwrap();
        let (flow, generation) = {
            let mut entry = line.lock();
            assert!(matches!(entry.flow, crate::IrqFlow::Fasteoi(_)));
            if has_action {
                assert!(
                    entry.install_regular_action(crate::action::IrqAction::regular(Arc::new(
                        |_| crate::IrqEvent::HANDLED
                    ),))
                );
            } else {
                entry.begin_dispatch();
            }
            (entry.flow, entry.generation_for_tests())
        };
        let result = state.try_resolve_desc(enriched, None);
        assert!(matches!(
            result,
            Err(IrqDescError::UnsupportedFlow { hwirq: 1 })
        ));
        {
            let entry = line.lock();
            assert_eq!(entry.desc, original);
            assert_eq!(entry.flow, flow);
            assert_eq!(entry.generation_for_tests(), generation);
        }
        assert_eq!(state.domain_virq(crate::PLIC_ROOT_DOMAIN, 1), Some(0x5f58));
        assert!(
            state
                .descs
                .get(&0x5f58)
                .is_some_and(|stored| Arc::ptr_eq(&stored, &line))
        );
        if has_action {
            let removed = line.lock().take_regular_action();
            assert!(removed.is_some());
            drop(removed);
        } else {
            let wake = line.lock().finish_dispatch();
            drop(wake);
        }
        state.try_resolve_desc(enriched, None).unwrap();
        assert_eq!(line.lock().desc, enriched);
        assert_eq!(line.lock().flow, flow);
        assert_eq!(line.lock().generation_for_tests(), generation + 1);
    }
}

#[def_test]
fn locked_resolution_preserves_line_and_plain_metadata() {
    let virq = 0x5f52;
    let desc = IrqDesc::from_virq(virq).with_flags(IrqFlags::MSI);
    let line = Arc::new(IrqLine::new(IrqStateDesc::new(desc)));
    let mut state = IrqState::new();
    let mut entry = line.lock();
    let resolved =
        try_resolve_and_publish_locked(&mut state, IrqSpec::Desc(desc), &line, &mut entry).unwrap();
    assert_eq!(resolved, desc);
    assert!(
        state
            .descs
            .get(&virq)
            .is_some_and(|stored| Arc::ptr_eq(&stored, &line))
    );
    let resolved =
        try_resolve_and_publish_locked(&mut state, IrqSpec::PlainVirq(virq), &line, &mut entry)
            .unwrap();
    assert_eq!(resolved, desc);
    assert_eq!(entry.desc, desc);
}

#[def_test]
fn locked_resolution_rejects_a_different_line_identity() {
    let virq = 0x5f53;
    let desc = IrqDesc::from_virq(virq);
    let line = Arc::new(IrqLine::new(IrqStateDesc::new(desc)));
    let other = Arc::new(IrqLine::new(IrqStateDesc::new(desc)));
    let mut state = IrqState::new();
    state.try_resolve_desc(desc, Some(&line)).unwrap();
    let mut entry = other.lock();
    let result = try_resolve_and_publish_locked(
        &mut state,
        IrqSpec::Desc(desc.with_flags(IrqFlags::MSI)),
        &other,
        &mut entry,
    );
    assert!(matches!(result, Err(IrqDescError::UnknownIrq)));
    assert_eq!(entry.desc, desc);
    assert_eq!(line.lock().desc, desc);
    assert!(
        state
            .descs
            .get(&virq)
            .is_some_and(|stored| Arc::ptr_eq(&stored, &line))
    );
}

#[def_test]
fn management_completion_updates_only_the_original_identity() {
    let virq = 0x5f54;
    let desc = IrqDesc::from_virq(virq);
    let old = Arc::new(IrqLine::new(IrqStateDesc::new(desc)));
    let successor = Arc::new(IrqLine::new(IrqStateDesc::new(desc)));
    let mut state = IrqState::new();
    state.try_resolve_desc(desc, Some(&old)).unwrap();
    old.lock().begin_management();
    old.lock().begin_teardown();
    // Model a stale identity defensively; ordinary admission prevents this
    // replacement, but completion must never clean up by number alone.
    state.descs.insert(virq, successor.clone());
    old.lock().finish_management(true);
    assert!(old.lock().is_unused());
    assert!(
        state
            .descs
            .get(&virq)
            .is_some_and(|line| Arc::ptr_eq(&line, &successor))
    );
    assert_eq!(Arc::strong_count(&old), 1);
}

#[def_test]
fn management_completion_keeps_in_flight_line_until_dispatch_finishes() {
    let virq = 0x5f55;
    let desc = IrqDesc::from_virq(virq);
    let line = Arc::new(IrqLine::new(IrqStateDesc::new(desc)));
    let mut state = IrqState::new();
    state.try_resolve_desc(desc, Some(&line)).unwrap();
    {
        let mut entry = line.lock();
        entry.begin_management();
        entry.begin_dispatch();
        entry.begin_teardown();
    }
    line.lock().finish_management(true);
    assert!(!line.lock().is_teardown_in_progress());
    assert!(!line.lock().is_unused());
    assert!(state.descs.contains_key(&virq));
    let wake = line.lock().finish_dispatch();
    drop(wake);
    assert_eq!(state.remove_if_unused(virq), Some(desc));
    assert_eq!(Arc::strong_count(&line), 1);
}

#[def_test]
fn management_completion_preserves_both_idle_and_drained_descriptors() {
    let virq = 0x5f56;
    let desc = IrqDesc::from_virq(virq);
    let line = Arc::new(IrqLine::new(IrqStateDesc::new(desc)));
    let mut state = IrqState::new();
    state.try_resolve_desc(desc, Some(&line)).unwrap();
    line.lock().begin_management();
    line.lock().finish_management(false);
    assert!(state.descs.contains_key(&virq));
    assert!(line.lock().is_unused());
    line.lock().begin_management();
    line.lock().begin_teardown();
    line.lock().finish_management(true);
    assert!(
        state
            .descs
            .get(&virq)
            .is_some_and(|stored| Arc::ptr_eq(&stored, &line))
    );
    assert!(line.lock().is_unused());
    assert_eq!(Arc::strong_count(&line), 2);
}

#[def_test]
fn management_admission_prevents_unused_line_removal() {
    let virq = 0x5f50;
    let desc = IrqDesc::from_virq(virq);
    let line = Arc::new(IrqLine::new(IrqStateDesc::new(desc)));
    let mut state = IrqState::new();
    state.try_resolve_desc(desc, Some(&line)).unwrap();

    assert!(line.lock().is_unused());
    line.lock().begin_management();
    assert!(!line.lock().is_unused());
    assert!(state.remove_if_unused(virq).is_none());
    assert!(
        state
            .descs
            .get(&virq)
            .is_some_and(|stored| Arc::ptr_eq(&stored, &line))
    );

    line.lock().end_management();
    assert!(line.lock().is_unused());
    assert_eq!(state.remove_if_unused(virq), Some(desc));
    assert!(!state.descs.contains_key(&virq));
    assert_eq!(Arc::strong_count(&line), 1);
}

#[def_test]
fn management_admission_prevents_unused_msi_removal() {
    let virq = 0x5f51;
    // No domain is needed to exercise MSI ownership removal; keep publication
    // and hardware backends out of this local-state admission test.
    let desc = IrqDesc::from_virq(virq).with_flags(IrqFlags::MSI);
    let line = Arc::new(IrqLine::new(IrqStateDesc::new(desc)));
    let mut state = IrqState::new();
    state.try_resolve_desc(desc, Some(&line)).unwrap();

    assert!(line.lock().is_unused());
    line.lock().begin_management();
    assert!(!line.lock().is_unused());
    assert!(state.remove_msi_if_unused(virq).is_none());
    assert!(
        state
            .descs
            .get(&virq)
            .is_some_and(|stored| Arc::ptr_eq(&stored, &line))
    );

    line.lock().end_management();
    assert!(line.lock().is_unused());
    assert_eq!(state.remove_msi_if_unused(virq), Some(desc.hwirq));
    assert!(!state.descs.contains_key(&virq));
    assert_eq!(Arc::strong_count(&line), 1);
}
