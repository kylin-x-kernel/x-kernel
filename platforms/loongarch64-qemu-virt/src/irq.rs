use kplat::interrupts::{Handler, HandlerTable, IntrManager, TargetCpu};
use loongArch64::reg_handler::{
    ecfg::{self, LineBasedInterrupt},
    ticlr,
};

use crate::config::devices::{EIOINTC_IRQ, TIMER_IRQ};
mod eiointc;
mod pch_pic;
pub const MAX_IRQ_COUNT: usize = 12;
static IRQ_HANDLER_TABLE: HandlerTable<MAX_IRQ_COUNT> = HandlerTable::new();
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
struct IntrManagerImpl;
#[impl_dev_interface]
impl IntrManager for IntrManagerImpl {
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

    fn reg_handler(irq: usize, handler: Handler) -> bool {
        if IRQ_HANDLER_TABLE.reg_handler_handler(irq, handler) {
            Self::enable(irq, true);
            return true;
        }
        warn!("reg_handler handler for IRQ {} failed", irq);
        false
    }

    fn unreg_handler(irq: usize) -> Option<Handler> {
        IRQ_HANDLER_TABLE
            .unreg_handler_handler(irq)
            .inspect(|_| Self::enable(irq, false))
    }

    fn dispatch_irq(irq: usize) -> Option<usize> {
        let mut irq = IrqType::new(irq);
        if matches!(irq, IrqType::Io) {
            let Some(ex_irq) = eiointc::claim_irq() else {
                debug!("Spurious external IRQ");
                return None;
            };
            irq = IrqType::Ex(ex_irq);
        }
        trace!("IRQ {irq:?}");
        if !IRQ_HANDLER_TABLE.dispatch_irq(irq.as_usize()) {
            debug!("Undispatch_irqd IRQ {irq:?}");
        }
        match irq {
            IrqType::Timer => {
                ticlr::clear_timer_interrupt();
            }
            IrqType::Io => {}
            IrqType::Ex(irq) => {
                eiointc::complete_irq(irq);
            }
        }
        Some(irq.as_usize())
    }

    fn notify_cpu(_interrupt_id: usize, _target: TargetCpu) {
        todo!()
    }

    fn set_prio(irq: usize, priority: u8) {
        todo!()
    }

    fn save_disable() -> usize {
        todo!()
    }

    fn restore(flag: usize) {
        todo!()
    }

    fn enable_local() {
        todo!()
    }

    fn disable_local() {
        todo!()
    }

    fn is_enabled() -> bool {
        todo!()
    }
}
