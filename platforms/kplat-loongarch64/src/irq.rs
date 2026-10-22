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

// External vectors target CPU0; an unpinned SMP test may run elsewhere.
#[cfg(not(feature = "smp"))]
unittest::path_mod_test!(tests, "../ut-tests/irq.rs");

pub(crate) fn init() {
    eiointc::init();
    pch_pic::init();
    // EIOINTC routes external vectors to CPU0 HWI1. Device masks remain
    // downstream; without this parent gate no enabled device can interrupt.
    ecfg::set_lie(ecfg::read().lie() | LineBasedInterrupt::HWI1);
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
#[kiface::provide]
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
                // Gate the external hwirq at the platform routing layer, before
                // it can reach either controller. The PCH PIC table (64 lines)
                // is the binding constraint, so any number outside
                // `0..PCH_PIC_IRQ_COUNT` is not a valid device interrupt for
                // this platform and is rejected rather than forwarded. The
                // controller-level checks in `pch_pic`/`eiointc` are
                // defense-in-depth.
                if irq >= pch_pic::PCH_PIC_IRQ_COUNT {
                    warn!(
                        "loongarch: reject out-of-range external hwirq {irq} (valid 0..{})",
                        pch_pic::PCH_PIC_IRQ_COUNT
                    );
                    return;
                }
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

    fn dispatch_irq(irq: usize) -> Option<kirq::Virq> {
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
        kirq::generic_handle_irq(kirq::PendingIrq::new(kirq::IrqRef::Virq(irq.as_usize()), 0))
    }

    fn dispatch_nmi(_irq: usize) {}

    fn complete_irq(_completion_cookie: usize) {}

    #[expect(
        clippy::todo,
        reason = "CG-023-unfinished-implementations: TODO(cg): LoongArch inter-processor \
                  interrupt delivery to the target CPU is not implemented"
    )]
    fn notify_cpu(_interrupt_id: usize, _target: TargetCpu) {
        todo!()
    }

    fn prepare_local_irq_work() -> Option<fn()> {
        None
    }

    #[expect(
        clippy::todo,
        reason = "CG-023-unfinished-implementations: TODO(cg): LoongArch controller-local \
                  interrupt priority programming is not implemented"
    )]
    fn set_prio(_irq: usize, _priority: u8) {
        todo!()
    }
}
