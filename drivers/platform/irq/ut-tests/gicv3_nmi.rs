// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::{IntId, Registers, update_locked};

#[derive(Default)]
struct Recorder {
    has_completed_disable: bool,
    is_enabled: bool,
    is_active: bool,
    is_pending: bool,
    has_stuck_pending: bool,
    accepts_attributes: bool,
    clears: usize,
    attributes: usize,
}

impl Registers for Recorder {
    fn wait_for_disable(&mut self) -> bool {
        self.has_completed_disable
    }

    fn is_enabled(&self, _: IntId) -> bool {
        self.is_enabled
    }

    fn is_active(&self, _: IntId) -> bool {
        self.is_active
    }

    fn clear_pending(&mut self, _: IntId) {
        self.clears += 1;
        self.is_pending = self.has_stuck_pending;
    }

    fn is_pending(&self, _: IntId) -> bool {
        self.is_pending
    }

    fn set_attributes(&mut self, _: IntId, _: bool) -> bool {
        self.attributes += 1;
        self.accepts_attributes
    }
}

#[def_test]
fn incomplete_disable_or_live_claim_cannot_change_nmi_attributes() {
    for (has_completed_disable, is_enabled, is_active) in [
        (false, false, false),
        (true, true, false),
        (true, false, true),
    ] {
        let mut registers = Recorder {
            has_completed_disable,
            is_enabled,
            is_active,
            accepts_attributes: true,
            ..Recorder::default()
        };
        assert!(update_locked(&mut registers, IntId::ppi(7), true).is_err());
        assert!(update_locked(&mut registers, IntId::ppi(7), false).is_err());
        assert_eq!(registers.clears, 0);
        assert_eq!(registers.attributes, 0);
    }
}

#[def_test]
fn teardown_clears_pending_before_demotion_and_retains_failure() {
    let mut registers = Recorder {
        has_completed_disable: true,
        is_pending: true,
        has_stuck_pending: true,
        accepts_attributes: true,
        ..Recorder::default()
    };
    assert!(update_locked(&mut registers, IntId::ppi(7), false).is_err());
    assert_eq!(registers.clears, 1);
    assert_eq!(registers.attributes, 0);
    registers.has_stuck_pending = false;
    registers.accepts_attributes = false;
    assert!(update_locked(&mut registers, IntId::ppi(7), false).is_err());
    assert_eq!(registers.attributes, 1);
    registers.accepts_attributes = true;
    assert!(update_locked(&mut registers, IntId::ppi(7), false).is_ok());
    assert_eq!(registers.attributes, 2);
    assert!(!registers.is_pending);
}

#[def_test]
fn preparation_preserves_pending_delivery() {
    let mut registers = Recorder {
        has_completed_disable: true,
        is_pending: true,
        accepts_attributes: true,
        ..Recorder::default()
    };
    assert!(update_locked(&mut registers, IntId::ppi(7), true).is_ok());
    assert_eq!(registers.clears, 0);
    assert_eq!(registers.attributes, 1);
    assert!(registers.is_pending);
}
