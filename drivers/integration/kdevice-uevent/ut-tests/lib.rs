// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn property_values_cannot_inject_uevent_fields() {
    assert_eq!(
        sanitized_property("keyboard\0ACTION=remove\n"),
        "keyboard ACTION=remove "
    );
}

#[def_test]
fn capability_bitmaps_use_linux_word_order() {
    assert_eq!(bitmap_value(&[1, 0, 0, 0, 2]), "200000001");
    assert_eq!(bitmap_value(&[0]), "0");
    assert_eq!(bitmap_value(&[]), "0");
}

#[def_test]
fn actions_accept_only_the_linux_kobject_set() {
    assert_eq!(UeventAction::try_from("change"), Ok(UeventAction::Change));
    assert_eq!(UeventAction::try_from("change\0remove"), Err(()));
}
