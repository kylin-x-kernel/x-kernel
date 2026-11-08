// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn pin_rejects_values_outside_the_dedicated_vector_window() {
    assert_eq!(MAX_IO_APIC_PINS, 32);
    assert_eq!(IoApicPin::MAX, 31);
    assert_eq!(IoApicPin::try_from_usize(0).unwrap().as_usize(), 0);
    assert_eq!(
        IoApicPin::try_from_usize(IoApicPin::MAX)
            .unwrap()
            .as_usize(),
        IoApicPin::MAX
    );
    assert!(IoApicPin::try_from_usize(MAX_IO_APIC_PINS).is_none());
    assert!(IoApicPin::try_from_usize(usize::MAX).is_none());
    assert!(IoApicPin::new(31).is_some());
    assert!(IoApicPin::new(32).is_none());
}

#[def_test]
fn legacy_policy_only_defaults_pins_from_ten_to_level_low() {
    let policy = IoApicInitPolicy::platform_legacy();
    assert_eq!(initial_redirection_flags(9, policy), IrqFlags::MASKED);
    assert_eq!(
        initial_redirection_flags(10, policy),
        IrqFlags::MASKED | IrqFlags::LEVEL_TRIGGERED | IrqFlags::LOW_ACTIVE
    );
    assert_eq!(
        initial_redirection_flags(31, policy),
        IrqFlags::MASKED | IrqFlags::LEVEL_TRIGGERED | IrqFlags::LOW_ACTIVE
    );

    let no_defaults = IoApicInitPolicy {
        default_level_low_from: None,
    };
    assert_eq!(initial_redirection_flags(23, no_defaults), IrqFlags::MASKED);
}

#[def_test]
fn capabilities_report_every_hardware_pin() {
    assert_eq!(IoApicCapabilities::from_max_table_entry(0).pin_count, 1);
    assert_eq!(IoApicCapabilities::from_max_table_entry(23).pin_count, 24);
    assert_eq!(
        IoApicCapabilities::from_max_table_entry(u8::MAX).pin_count,
        256
    );
}

#[def_test]
fn gsi_resolution_uses_range_arithmetic_not_numeric_identity() {
    assert_eq!(pin_for_gsi_in_range(4, 0, 24).map(IoApicPin::as_u8), Some(4));
    assert_eq!(pin_for_gsi_in_range(0, 24, 24), None);
    assert_eq!(
        pin_for_gsi_in_range(24, 24, 24).map(IoApicPin::as_u8),
        Some(0)
    );
    assert_eq!(
        pin_for_gsi_in_range(28, 24, 24).map(IoApicPin::as_u8),
        Some(4)
    );
    assert_eq!(pin_for_gsi_in_range(47, 24, 24).map(IoApicPin::as_u8), Some(23));
    assert_eq!(pin_for_gsi_in_range(48, 24, 24), None);
}

#[def_test]
fn gsi_resolution_rejects_offsets_without_a_fixed_vector() {
    assert_eq!(pin_for_gsi_in_range(31, 0, 32).map(IoApicPin::as_u8), Some(31));
    assert_eq!(pin_for_gsi_in_range(32, 0, 32), None);
    // A controller can report more pins than the fixed IO-APIC vector window.
    assert_eq!(pin_for_gsi_in_range(32, 0, 256), None);
    assert_eq!(pin_for_gsi_in_range(u32::MAX, 1, 32), None);
}

#[def_test]
fn instance_ids_cover_only_the_configured_capacity() {
    assert_eq!(MAX_IO_APICS, 32);
    assert_eq!(
        IoApicInstanceId::try_from_usize(0)
            .expect("first instance")
            .as_usize(),
        0
    );
    assert_eq!(
        IoApicInstanceId::try_from_usize(MAX_IO_APICS - 1)
            .expect("last instance")
            .as_usize(),
        MAX_IO_APICS - 1
    );
    assert!(IoApicInstanceId::try_from_usize(MAX_IO_APICS).is_none());
    assert!(IoApicInstanceId::try_from_usize(usize::MAX).is_none());
}
