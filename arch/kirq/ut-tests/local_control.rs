// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::{IrqDesc, LocalControl};

#[def_test]
fn local_enable_depth_and_configuration_are_independent() {
    let desc = IrqDesc::from_virq(0x5f80);
    // Explicit CPU identities exercise the protocol even in a UP test build.
    let mut slots = [LocalControl::new(); 2];
    for (cpu, slot) in slots.iter_mut().enumerate() {
        let plan = slot.enable(desc, 7, cpu, false, false);
        assert_eq!(plan.cpu, Some(cpu));
        assert!(plan.configure);
        assert_eq!(plan.enable, Some(true));
        slot.commit(plan, 7);
    }
    for _ in 0..2 {
        let plan = slots[0].disable(desc, 0);
        slots[0].commit(plan, 7);
    }
    assert!(!slots[0].can_run());
    assert!(slots[1].can_run());
    let nested = slots[0].enable(desc, 7, 0, false, false);
    assert_eq!(nested.enable, None);
    slots[0].commit(nested, 7);
    assert!(!slots[0].can_run());
    let final_enable = slots[0].enable(desc, 7, 0, false, false);
    assert!(!final_enable.configure);
    assert_eq!(final_enable.enable, Some(true));
    slots[0].commit(final_enable, 7);
    assert!(slots[0].can_run());
    assert!(slots[1].can_run());
}

#[def_test]
fn disabled_registration_masks_only_the_enabled_local_slot() {
    let desc = IrqDesc::from_virq(0x5f85);
    let mut slots = [LocalControl::new(); 2];
    for (cpu, slot) in slots.iter_mut().enumerate() {
        let plan = slot.enable(desc, 1, cpu, true, false);
        slot.commit(plan, 1);
    }
    let disabled = slots[0].register_disabled(desc, 0);
    assert_eq!(disabled.cpu, Some(0));
    assert_eq!(disabled.enable, Some(false));
    slots[0].commit(disabled, 1);
    assert!(!slots[0].is_enabled);
    assert!(!slots[0].can_run());
    assert!(slots[1].can_run());

    let nested = slots[0].disable(desc, 0);
    assert_eq!(nested.enable, None);
    slots[0].commit(nested, 1);
    let registered_again = slots[0].register_disabled(desc, 0);
    assert_eq!(registered_again.enable, None);
    slots[0].commit(registered_again, 1);
    let still_disabled = slots[0].enable(desc, 1, 0, false, false);
    assert_eq!(still_disabled.enable, None);
    slots[0].commit(still_disabled, 1);
    assert!(!slots[0].can_run());
    let enabled = slots[0].enable(desc, 1, 0, false, false);
    assert_eq!(enabled.enable, Some(true));
    slots[0].commit(enabled, 1);
    assert!(slots[0].can_run());
    assert!(slots[1].can_run());
}

#[def_test]
fn failed_local_configuration_does_not_poison_another_cpu() {
    let desc = IrqDesc::from_virq(0x5f81);
    let mut slots = [LocalControl::new(); 2];
    for (cpu, slot) in slots.iter_mut().enumerate() {
        let plan = slot.enable(desc, 1, cpu, false, true);
        slot.commit(plan, 1);
    }
    slots[0].has_failed_configuration = true;
    assert!(!slots[0].can_run());
    assert!(slots[1].can_run());
    let retry = slots[0].enable(desc, 1, 0, false, false);
    assert!(retry.configure);
    assert_eq!(retry.enable, Some(true));
    slots[0].commit(retry, 1);
    assert!(slots[0].can_run());
    assert!(!slots[1].reconfigure(desc, 1, 1).configure);
    assert!(slots[1].reconfigure(desc, 2, 1).configure);
}
