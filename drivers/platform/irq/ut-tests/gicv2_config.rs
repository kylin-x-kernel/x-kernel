// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use alloc::vec::Vec;

use super::{ConfigRegisters, IntId, Trigger, checked_config, configure_locked};
use kirq::{IrqDescError, IrqTrigger};
use unittest::{assert_eq, def_test};

#[derive(Debug, Eq, PartialEq)]
enum Access {
    ReadConfig,
    ReadEnable,
    WriteConfig(Trigger),
    WriteEnable(bool),
}

struct FakeRegisters {
    trigger: Trigger,
    is_enabled: bool,
    ignores_config: bool,
    ignores_mask: bool,
    ignores_restore: bool,
    accesses: Vec<Access>,
}

impl FakeRegisters {
    fn new(trigger: Trigger, is_enabled: bool) -> Self {
        Self {
            trigger,
            is_enabled,
            ignores_config: false,
            ignores_mask: false,
            ignores_restore: false,
            accesses: Vec::new(),
        }
    }
}

impl ConfigRegisters for FakeRegisters {
    fn get_cfg(&mut self, _: IntId) -> Trigger {
        self.accesses.push(Access::ReadConfig);
        self.trigger
    }

    fn set_cfg(&mut self, _: IntId, trigger: Trigger) {
        self.accesses.push(Access::WriteConfig(trigger));
        if !self.ignores_config {
            self.trigger = trigger;
        }
    }

    fn is_irq_enable(&mut self, _: IntId) -> bool {
        self.accesses.push(Access::ReadEnable);
        self.is_enabled
    }

    fn set_irq_enable(&mut self, _: IntId, enabled: bool) {
        self.accesses.push(Access::WriteEnable(enabled));
        if !(enabled && self.ignores_restore || !enabled && self.ignores_mask) {
            self.is_enabled = enabled;
        }
    }
}

#[def_test]
fn checked_v2_configuration_preserves_enable_and_orders_writes() {
    use Access::*;
    for (old, new) in [(Trigger::Level, Trigger::Edge), (Trigger::Edge, Trigger::Level)] {
        let mut hw = FakeRegisters::new(old, true);
        assert_eq!(configure_locked(&mut hw, IntId::spi(0), new), Ok(()));
        assert_eq!(hw.trigger, new);
        assert_eq!(hw.is_enabled, true);
        assert_eq!(hw.accesses, [ReadConfig, ReadEnable, WriteEnable(false),
            ReadEnable, WriteConfig(new), ReadConfig, WriteEnable(true), ReadEnable]);

        let mut hw = FakeRegisters::new(old, false);
        assert_eq!(configure_locked(&mut hw, IntId::spi(0), new), Ok(()));
        assert_eq!(hw.is_enabled, false);
        assert_eq!(hw.accesses, [ReadConfig, ReadEnable, WriteConfig(new), ReadConfig]);
    }
}

#[def_test]
fn checked_v2_mismatch_keeps_source_masked_without_rollback() {
    use Access::*;
    for intid in [IntId::ppi(0), IntId::spi(0)] {
        for is_enabled in [false, true] {
            let mut hw = FakeRegisters::new(Trigger::Level, is_enabled);
            hw.ignores_config = true;
            assert_eq!(configure_locked(&mut hw, intid, Trigger::Edge),
                Err(IrqDescError::ConfigurationFailed { hwirq: intid.to_u32() as usize }));
            assert_eq!(hw.is_enabled, false);
            assert_eq!(hw.accesses.last(), Some(&WriteEnable(false)));
            assert_eq!(hw.accesses.contains(&WriteEnable(true)), false);
            assert_eq!(hw.accesses.contains(&WriteConfig(Trigger::Level)), false);
        }
    }
}

#[def_test]
fn checked_v2_failed_mask_never_programs_trigger() {
    use Access::*;
    let mut hw = FakeRegisters::new(Trigger::Level, true);
    hw.ignores_mask = true;
    assert_eq!(configure_locked(&mut hw, IntId::spi(0), Trigger::Edge),
        Err(IrqDescError::ConfigurationFailed { hwirq: 32 }));
    assert_eq!(hw.accesses, [ReadConfig, ReadEnable, WriteEnable(false),
        ReadEnable, WriteEnable(false)]);
    // Failed hardware masking is reported, not described as fail-safe isolation.
    assert_eq!(hw.is_enabled, true);
}

#[def_test]
fn checked_v2_failed_enable_restore_requests_mask() {
    use Access::*;
    let mut hw = FakeRegisters::new(Trigger::Level, true);
    hw.ignores_restore = true;
    assert_eq!(configure_locked(&mut hw, IntId::spi(0), Trigger::Edge),
        Err(IrqDescError::ConfigurationFailed { hwirq: 32 }));
    assert_eq!(hw.trigger, Trigger::Edge);
    assert_eq!(hw.is_enabled, false);
    assert_eq!(hw.accesses, [ReadConfig, ReadEnable, WriteEnable(false), ReadEnable,
        WriteConfig(Trigger::Edge), ReadConfig, WriteEnable(true), ReadEnable,
        WriteEnable(false)]);
}

#[def_test]
fn checked_v2_fixed_matching_ppi_needs_no_writes() {
    for is_enabled in [false, true] {
        let mut hw = FakeRegisters::new(Trigger::Level, is_enabled);
        hw.ignores_config = true;
        assert_eq!(configure_locked(&mut hw, IntId::ppi(0), Trigger::Level), Ok(()));
        assert_eq!(hw.is_enabled, is_enabled);
        assert_eq!(hw.accesses, [Access::ReadConfig]);
    }
}

#[def_test]
fn checked_v2_admission_handles_fixed_sgi_and_unknown_trigger() {
    for hwirq in [0, 15] {
        assert_eq!(checked_config(kirq::gic_edge_irq_desc(hwirq), Some(64)), Ok(None));
        for trigger in [IrqTrigger::LevelHigh, IrqTrigger::LevelLow, IrqTrigger::EdgeFalling] {
            assert_eq!(checked_config(kirq::gic_irq_desc(hwirq, trigger), Some(64)),
                Err(IrqDescError::UnsupportedFlow { hwirq }));
        }
    }
    for hwirq in [0, 16, 32] {
        assert_eq!(checked_config(kirq::gic_irq_desc(hwirq, IrqTrigger::Unknown(0)), Some(64)),
            Ok(None));
    }
    for (hwirq, limit) in [(32, None), (64, Some(64)), (1020, Some(1024)),
        (usize::MAX, Some(64))] {
        assert_eq!(checked_config(kirq::gic_edge_irq_desc(hwirq), limit),
            Err(IrqDescError::UnsupportedFlow { hwirq }));
    }
    for trigger in [IrqTrigger::EdgeFalling, IrqTrigger::LevelLow] {
        assert_eq!(checked_config(kirq::gic_irq_desc(32, trigger), Some(64)),
            Err(IrqDescError::UnsupportedFlow { hwirq: 32 }));
    }
}
