// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, def_test};

#[def_test]
fn external_interrupt_parent_is_enabled_after_boot() {
    // Read boot's persistent gate without changing live interrupt state.
    assert!(
        super::ecfg::read()
            .lie()
            .contains(super::LineBasedInterrupt::HWI1)
    );
}
