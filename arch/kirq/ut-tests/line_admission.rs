// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use unittest::{assert, assert_eq, def_test};

use super::{IrqDesc, IrqFlags, IrqLine, IrqState, IrqStateDesc};

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
