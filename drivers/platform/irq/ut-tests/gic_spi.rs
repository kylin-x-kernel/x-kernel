// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use alloc::vec::Vec;

use arm_gic_driver::IntId;
use unittest::{assert_eq, def_test};

#[def_test]
fn public_chip_operations_reject_non_spi_bindings_before_register_access() {
    use kirq::{FasteoiResend, IrqFlags};
    let FasteoiResend::HardwareOnly(retrigger) = super::CHIP.resend else {
        panic!("GIC SPI requires hardware-only retrigger");
    };
    for desc in [
        kirq::gic_edge_irq_desc(27),
        kirq::gic_edge_irq_desc(1020),
        kirq::gic_edge_irq_desc(usize::MAX),
        kirq::gic_edge_irq_desc(32).with_flags(IrqFlags::PER_CPU),
        kirq::gic_edge_irq_desc(32).with_domain(kirq::PLIC_ROOT_DOMAIN),
        kirq::IrqDesc::from_virq(32),
    ] {
        // Any regression would reach private, unowned or out-of-range hardware.
        // Assert the pure gate first so a failing test never performs that write.
        assert_eq!(super::is_spi_binding(desc), false);
        (super::CHIP.mask)(desc);
        (super::CHIP.unmask)(desc);
        assert_eq!(retrigger(desc), false);
    }
}

#[derive(Debug, Eq, PartialEq)]
enum Access {
    Enable(u32, bool),
    WaitDisable,
    Pending(u32),
}

struct Registers {
    accesses: Vec<Access>,
    has_timeout: bool,
}

impl super::Registers for Registers {
    fn set_enabled(&mut self, intid: IntId, enabled: bool) {
        self.accesses.push(Access::Enable(intid.to_u32(), enabled));
    }

    fn wait_for_disable(&mut self) -> bool {
        self.accesses.push(Access::WaitDisable);
        !self.has_timeout
    }

    fn set_pending(&mut self, intid: IntId) {
        self.accesses.push(Access::Pending(intid.to_u32()));
    }
}

#[def_test]
fn spi_mask_waits_after_disable_and_reports_timeout_without_restore() {
    for has_timeout in [false, true] {
        let mut registers = Registers {
            accesses: Vec::new(),
            has_timeout,
        };
        assert_eq!(
            super::set_enabled(&mut registers, IntId::spi(17), false),
            !has_timeout
        );
        assert_eq!(
            registers.accesses,
            [Access::Enable(49, false), Access::WaitDisable]
        );
    }
}

#[def_test]
fn spi_unmask_does_not_wait_or_change_pending_priority_and_routing() {
    let mut registers = Registers {
        accesses: Vec::new(),
        has_timeout: true,
    };
    assert_eq!(
        super::set_enabled(&mut registers, IntId::spi(0), true),
        true
    );
    assert_eq!(registers.accesses, [Access::Enable(32, true)]);
}

#[def_test]
fn spi_retrigger_issues_only_pending_write_without_mask_or_completion() {
    let mut registers = Registers {
        accesses: Vec::new(),
        has_timeout: true,
    };
    super::set_pending(&mut registers, IntId::spi(987));
    assert_eq!(registers.accesses, [Access::Pending(1019)]);
}
