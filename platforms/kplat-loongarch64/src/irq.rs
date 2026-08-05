// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kirq::TargetCpu;
use loongArch64::register::{
    ecfg::{self, LineBasedInterrupt},
    ticlr,
};

const TIMER_IRQ: usize = 11;
const EIOINTC_IRQ: usize = 3;

mod eiointc;
mod pch_pic;
pub(crate) fn init() {
    eiointc::init();
    pch_pic::init();
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IrqType {
    Timer,
    Io,
    Ex(usize),
}
impl IrqType {
    fn new(irq: usize) -> Self {
        match irq {
            TIMER_IRQ => Self::Timer,
            EIOINTC_IRQ => Self::Io,
            n => Self::Ex(n),
        }
    }

    fn as_usize(&self) -> usize {
        match self {
            IrqType::Timer => TIMER_IRQ,
            IrqType::Io => EIOINTC_IRQ,
            IrqType::Ex(n) => *n,
        }
    }
}
#[impl_dev_interface]
impl kirq::IntrManagerIf {
    fn configure(_desc: kirq::IrqDesc) {}

    fn enable(irq: usize, enabled: bool) {
        let irq = IrqType::new(irq);
        match irq {
            IrqType::Timer => {
                let old_value = ecfg::read().lie();
                let new_value = match enabled {
                    true => old_value | LineBasedInterrupt::TIMER,
                    false => old_value & !LineBasedInterrupt::TIMER,
                };
                ecfg::set_lie(new_value);
            }
            IrqType::Io => {}
            IrqType::Ex(irq) => {
                if enabled {
                    eiointc::enable_irq(irq);
                    pch_pic::enable_irq(irq);
                } else {
                    eiointc::disable_irq(irq);
                    pch_pic::disable_irq(irq);
                }
            }
        }
    }

    fn dispatch_irq(irq: usize) -> Option<kirq::PendingIrq> {
        let mut irq = IrqType::new(irq);
        if matches!(irq, IrqType::Io) {
            let Some(ex_irq) = eiointc::claim_irq() else {
                debug!("Spurious external IRQ");
                return None;
            };
            irq = IrqType::Ex(ex_irq);
        }
        trace!("IRQ {irq:?}");
        match irq {
            IrqType::Timer => {
                ticlr::clear_timer_interrupt();
            }
            IrqType::Io => {}
            IrqType::Ex(irq) => {
                eiointc::complete_irq(irq);
            }
        }
        Some(kirq::PendingIrq::new(kirq::IrqRef::Virq(irq.as_usize()), 0))
    }

    fn dispatch_nmi(_irq: usize) -> Option<kirq::DispatchedIrq> {
        None
    }

    fn complete_irq(_completion_cookie: usize) {}

    fn notify_cpu(_interrupt_id: usize, _target: TargetCpu) {
        todo!()
    }

    fn set_prio(_irq: usize, _priority: u8) {
        todo!()
    }
}
