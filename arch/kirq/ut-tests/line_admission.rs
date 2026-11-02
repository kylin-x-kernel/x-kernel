// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use unittest::{assert, assert_eq, def_test};

use super::{IrqDesc, IrqFlags, IrqLine, IrqState, IrqStateDesc, try_resolve_and_publish_locked};
use crate::{IrqDescError, IrqSpec};

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
