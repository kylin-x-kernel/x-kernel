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
    WaitDistributor,
}

struct FakeRegisters {
    trigger: Trigger,
    is_enabled: bool,
    has_rwp_timeout: bool,
    ignores_config: bool,
    ignores_mask: bool,
    ignores_restore: bool,
    accesses: Vec<Access>,
}

impl FakeRegisters {
    fn new(is_enabled: bool) -> Self {
        Self {
            trigger: Trigger::Level,
            is_enabled,
            has_rwp_timeout: false,
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

    fn wait_for_distributor_rwp(&mut self) -> bool {
        self.accesses.push(Access::WaitDistributor);
        !self.has_rwp_timeout
    }
}

#[def_test]
fn checked_v3_spi_masks_waits_configures_and_restores() {
    use Access::*;
    let mut hw = FakeRegisters::new(true);
    assert_eq!(configure_locked(&mut hw, IntId::spi(0), Trigger::Edge), Ok(()));
    assert_eq!(hw.is_enabled, true);
    assert_eq!(hw.accesses, [ReadConfig, ReadEnable, WriteEnable(false), WaitDistributor,
        ReadEnable, WriteConfig(Trigger::Edge), ReadConfig, WriteEnable(true), ReadEnable]);

    let mut hw = FakeRegisters::new(false);
    assert_eq!(configure_locked(&mut hw, IntId::spi(0), Trigger::Edge), Ok(()));
    assert_eq!(hw.is_enabled, false);
    assert_eq!(hw.accesses, [ReadConfig, ReadEnable, WaitDistributor, ReadEnable,
        WriteConfig(Trigger::Edge), ReadConfig]);
}

#[def_test]
fn checked_v3_timeout_stops_before_trigger_write_and_restore() {
    use Access::*;
    for is_enabled in [false, true] {
        let mut hw = FakeRegisters::new(is_enabled);
        hw.has_rwp_timeout = true;
        assert_eq!(configure_locked(&mut hw, IntId::spi(0), Trigger::Edge),
            Err(IrqDescError::ConfigurationFailed { hwirq: 32 }));
        assert_eq!(hw.accesses.contains(&WriteConfig(Trigger::Edge)), false);
        assert_eq!(hw.accesses.contains(&WriteEnable(true)), false);
        assert_eq!(hw.accesses.last(), Some(&WriteEnable(false)));
        assert_eq!(hw.is_enabled, false);
    }
}

#[def_test]
fn checked_v3_mask_failure_stops_before_trigger_write() {
    use Access::*;
    let mut hw = FakeRegisters::new(true);
    hw.ignores_mask = true;
    assert_eq!(configure_locked(&mut hw, IntId::spi(0), Trigger::Edge),
        Err(IrqDescError::ConfigurationFailed { hwirq: 32 }));
    assert_eq!(hw.accesses, [ReadConfig, ReadEnable, WriteEnable(false), WaitDistributor,
        ReadEnable, WriteEnable(false)]);
    assert_eq!(hw.is_enabled, true);
}

#[def_test]
fn checked_v3_mismatch_never_restores_enable() {
    use Access::*;
    for is_enabled in [false, true] {
        let mut hw = FakeRegisters::new(is_enabled);
        hw.ignores_config = true;
        assert_eq!(configure_locked(&mut hw, IntId::spi(0), Trigger::Edge),
            Err(IrqDescError::ConfigurationFailed { hwirq: 32 }));
        assert_eq!(hw.is_enabled, false);
        assert_eq!(hw.accesses.contains(&WriteEnable(true)), false);
        assert_eq!(hw.accesses.contains(&WriteConfig(Trigger::Level)), false);
        assert_eq!(hw.accesses.last(), Some(&WriteEnable(false)));
    }
}

#[def_test]
fn checked_v3_restore_failure_requests_mask_without_rollback() {
    let mut hw = FakeRegisters::new(true);
    hw.ignores_restore = true;
    assert_eq!(configure_locked(&mut hw, IntId::spi(0), Trigger::Edge),
        Err(IrqDescError::ConfigurationFailed { hwirq: 32 }));
    assert_eq!(hw.is_enabled, false);
    assert_eq!(hw.trigger, Trigger::Edge);
    assert_eq!(hw.accesses.last(), Some(&Access::WriteEnable(false)));
}

#[def_test]
fn checked_v3_private_startup_only_reads_current_configuration() {
    for (intid, trigger) in [(IntId::sgi(15), Trigger::Edge), (IntId::ppi(0), Trigger::Level)] {
        for is_enabled in [false, true] {
            let mut hw = FakeRegisters::new(is_enabled);
            hw.trigger = trigger;
            assert_eq!(configure_locked(&mut hw, intid, trigger), Ok(()));
            assert_eq!(hw.accesses, [Access::ReadConfig]);
            assert_eq!(hw.is_enabled, is_enabled);

            hw.accesses.clear();
            let different = if trigger == Trigger::Edge { Trigger::Level } else { Trigger::Edge };
            assert_eq!(configure_locked(&mut hw, intid, different),
                Err(IrqDescError::UnsupportedFlow { hwirq: intid.to_u32() as usize }));
            assert_eq!(hw.accesses, [Access::ReadConfig]);
            assert_eq!(hw.trigger, trigger);
            assert_eq!(hw.is_enabled, is_enabled);
        }
    }
}

#[def_test]
fn checked_v3_admission_rejects_impossible_sgi_before_access() {
    for trigger in [IrqTrigger::LevelHigh, IrqTrigger::LevelLow, IrqTrigger::EdgeFalling] {
        assert_eq!(checked_config(kirq::gic_irq_desc(15, trigger), Some(64)),
            Err(IrqDescError::UnsupportedFlow { hwirq: 15 }));
    }
    assert_eq!(checked_config(kirq::gic_edge_irq_desc(15), Some(64)),
        Ok(Some((IntId::sgi(15), Trigger::Edge))));
    assert_eq!(checked_config(kirq::gic_irq_desc(32, IrqTrigger::Unknown(0)), Some(64)),
        Ok(None));
    assert_eq!(checked_config(kirq::gic_edge_irq_desc(64), Some(64)),
        Err(IrqDescError::UnsupportedFlow { hwirq: 64 }));
}
