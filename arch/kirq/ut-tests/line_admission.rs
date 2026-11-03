// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use unittest::{assert, assert_eq, def_test};

use super::{IrqDesc, IrqFlags, IrqLine, IrqState, IrqStateDesc, try_resolve_and_publish_locked};
use crate::{IrqDescError, IrqSpec};

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
                assert!(entry.install_regular_action(crate::action::IrqAction::regular(
                    Arc::new(|_| crate::IrqEvent::HANDLED),
                )));
            } else {
                entry.begin_dispatch();
            }
            entry.flow
        };
        assert!(matches!(state.try_resolve_desc(private, None),
            Err(IrqDescError::UnsupportedFlow { hwirq: 32 })));
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
        assert!(state.descs.get(&0x5f59).is_some_and(|stored| Arc::ptr_eq(stored, &line)));
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
    assert_eq!(crate::IntrManagerIf::select_flow(desc), Ok(crate::IrqFlow::Legacy));
    static EDGE: crate::EdgeChip = crate::EdgeChip {
        mask: |_| {}, unmask: |_| {}, retrigger: |_| true,
    };
    static LEVEL: crate::LevelChip = crate::LevelChip { mask_ack: |_| {}, unmask: |_| {} };
    for old_flow in [crate::IrqFlow::Fasteoi(&CHIP), crate::IrqFlow::Edge(&EDGE), crate::IrqFlow::Level(&LEVEL)] {
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
                assert!(entry.install_regular_action(crate::action::IrqAction::regular(
                    Arc::new(|_| crate::IrqEvent::HANDLED),
                )));
            } else {
                entry.begin_dispatch();
            }
            entry.generation_for_tests()
        };
        let result = state.try_resolve_desc(desc, None);
        assert!(matches!(result, Err(IrqDescError::UnsupportedFlow { hwirq }) if hwirq == desc.hwirq));
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
        assert!(state.descs.get(&desc.hwirq).is_some_and(|stored| Arc::ptr_eq(stored, &line)));
    }
    }
}

// The initialized RISC-V provider selects actual PLIC Fasteoi operations.
// Only a private registry is changed; no mapping snapshot or MMIO is published.
#[cfg(target_arch = "riscv64")]
#[def_test]
fn plic_metadata_enrichment_waits_for_actions_and_in_flight_admission() {
    let original = crate::plic_irq_desc(1).with_virq(0x5f58);
    let enriched = IrqDesc { trigger: crate::IrqTrigger::LevelHigh, ..original };
    for has_action in [true, false] {
        let mut state = IrqState::new();
        let line = Arc::new(IrqLine::new(IrqStateDesc::new(original)));
        state.try_resolve_desc(original, Some(&line)).unwrap();
        let (flow, generation) = {
            let mut entry = line.lock();
            assert!(matches!(entry.flow, crate::IrqFlow::Fasteoi(_)));
            if has_action {
                assert!(entry.install_regular_action(crate::action::IrqAction::regular(
                    Arc::new(|_| crate::IrqEvent::HANDLED),
                )));
            } else {
                entry.begin_dispatch();
            }
            (entry.flow, entry.generation_for_tests())
        };
        let result = state.try_resolve_desc(enriched, None);
        assert!(matches!(result, Err(IrqDescError::UnsupportedFlow { hwirq: 1 })));
        {
            let entry = line.lock();
            assert_eq!(entry.desc, original);
            assert_eq!(entry.flow, flow);
            assert_eq!(entry.generation_for_tests(), generation);
        }
        assert_eq!(state.domain_virq(crate::PLIC_ROOT_DOMAIN, 1), Some(0x5f58));
        assert!(state.descs.get(&0x5f58).is_some_and(|stored| Arc::ptr_eq(stored, &line)));
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
    let resolved = try_resolve_and_publish_locked(
        &mut state, IrqSpec::Desc(desc), &line, &mut entry,
    ).unwrap();
    assert_eq!(resolved, desc);
    assert!(state.descs.get(&virq).is_some_and(|stored| Arc::ptr_eq(stored, &line)));
    let resolved = try_resolve_and_publish_locked(
        &mut state, IrqSpec::PlainVirq(virq), &line, &mut entry,
    ).unwrap();
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
        &mut state, IrqSpec::Desc(desc.with_flags(IrqFlags::MSI)), &other, &mut entry,
    );
    assert!(matches!(result, Err(IrqDescError::UnknownIrq)));
    assert_eq!(entry.desc, desc);
    assert_eq!(line.lock().desc, desc);
    assert!(state.descs.get(&virq).is_some_and(|stored| Arc::ptr_eq(stored, &line)));
}

#[def_test]
fn management_completion_retires_only_the_original_identity() {
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
    state.finish_management(Some(virq), &old, true);
    assert!(old.lock().is_unused());
    assert!(state.descs.get(&virq).is_some_and(|line| Arc::ptr_eq(line, &successor)));
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
    state.finish_management(Some(virq), &line, true);
    assert!(!line.lock().is_teardown_in_progress());
    assert!(!line.lock().is_unused());
    assert!(state.descs.contains_key(&virq));
    let wake = line.lock().finish_dispatch();
    drop(wake);
    assert_eq!(state.remove_if_unused(virq), Some(desc));
    assert_eq!(Arc::strong_count(&line), 1);
}

#[def_test]
fn management_completion_preserves_non_removal_and_retires_drained_line() {
    let virq = 0x5f56;
    let desc = IrqDesc::from_virq(virq);
    let line = Arc::new(IrqLine::new(IrqStateDesc::new(desc)));
    let mut state = IrqState::new();
    state.try_resolve_desc(desc, Some(&line)).unwrap();
    line.lock().begin_management();
    state.finish_management(Some(virq), &line, false);
    assert!(state.descs.contains_key(&virq));
    assert!(line.lock().is_unused());
    line.lock().begin_management();
    line.lock().begin_teardown();
    state.finish_management(Some(virq), &line, true);
    assert!(!state.descs.contains_key(&virq));
    assert!(line.lock().is_unused());
    assert_eq!(Arc::strong_count(&line), 1);
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
    assert!(state.descs.get(&virq).is_some_and(|stored| Arc::ptr_eq(stored, &line)));

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
    assert!(state.descs.get(&virq).is_some_and(|stored| Arc::ptr_eq(stored, &line)));

    line.lock().end_management();
    assert!(line.lock().is_unused());
    assert_eq!(state.remove_msi_if_unused(virq), Some(desc.hwirq));
    assert!(!state.descs.contains_key(&virq));
    assert_eq!(Arc::strong_count(&line), 1);
}
