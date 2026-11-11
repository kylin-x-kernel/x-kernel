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

static EXTERNAL_LEVEL: kirq::LevelChip = kirq::LevelChip {
    mask_ack: mask_external,
    unmask: unmask_external,
};

fn mask_external(desc: kirq::IrqDesc) {
    // Close the producer before its parent. Level PCH sources have no leaf
    // ack; EIOINTC ISR was already consumed by the parent dispatch path.
    pch_pic::disable_irq(desc.hwirq);
    eiointc::disable_irq(desc.hwirq);
}

fn unmask_external(desc: kirq::IrqDesc) {
    pch_pic::clear_cause(desc.hwirq);
    eiointc::enable_irq(desc.hwirq);
    pch_pic::enable_irq(desc.hwirq);
}

fn claim_external(
    claim: impl FnOnce() -> Option<usize>,
    complete: impl FnOnce(usize),
) -> Option<usize> {
    let irq = claim()?;
    // Consume the parent ISR even for an unsupported leaf, before KIRQ lookup.
    // This is not the leaf's mask/ack operation and has no tail obligation.
    complete(irq);
    if irq >= pch_pic::PCH_PIC_IRQ_COUNT || matches!(irq, TIMER_IRQ | EIOINTC_IRQ) {
        warn!("loongarch: unsupported external vector {irq}");
        return None;
    }
    Some(irq)
}

fn select_flow(desc: kirq::IrqDesc) -> Result<kirq::IrqFlow, kirq::IrqDescError> {
    use kirq::{IrqAffinity, IrqController, IrqFlags, IrqTrigger};
    // Only the timer is a registerable local source. The EIOINTC parent is
    // demultiplexed before generic dispatch and cannot own an action.
    if desc.domain.is_none() && desc.controller == IrqController::Unknown && desc.hwirq == TIMER_IRQ
    {
        return Ok(kirq::IrqFlow::PerCpu);
    }
    if desc.domain.is_some()
        || desc.hwirq >= pch_pic::PCH_PIC_IRQ_COUNT
        || matches!(desc.hwirq, TIMER_IRQ | EIOINTC_IRQ)
        || !matches!(
            desc.controller,
            IrqController::Unknown | IrqController::LoongArchExtioi
        )
        || desc.affinity != IrqAffinity::Any
        || !(desc.flags - (IrqFlags::SHARED | IrqFlags::WAKEUP_SOURCE)).is_empty()
        || !matches!(
            desc.trigger,
            IrqTrigger::LevelHigh | IrqTrigger::LevelLow | IrqTrigger::Unknown(_)
        )
    {
        return Err(kirq::IrqDescError::UnsupportedFlow { hwirq: desc.hwirq });
    }
    Ok(kirq::IrqFlow::Level(&EXTERNAL_LEVEL))
}

fn configure(desc: kirq::IrqDesc) -> Result<(), kirq::IrqDescError> {
    configure_with(desc, pch_pic::configure_level)
}

fn configure_with(
    desc: kirq::IrqDesc,
    configure_external: impl FnOnce(usize, bool) -> bool,
) -> Result<(), kirq::IrqDescError> {
    // Only external level sources own PCH registers. CPU-local timer IDs
    // share this flat namespace but must never configure an external pin.
    if !matches!(select_flow(desc)?, kirq::IrqFlow::Level(_)) {
        return Ok(());
    }
    let is_low = match desc.polarity {
        kirq::IrqPolarity::Low => true,
        kirq::IrqPolarity::High => false,
        kirq::IrqPolarity::Unknown => desc.trigger == kirq::IrqTrigger::LevelLow,
    };
    if configure_external(desc.hwirq, is_low) {
        Ok(())
    } else {
        Err(kirq::IrqDescError::ConfigurationFailed { hwirq: desc.hwirq })
    }
}

// External vectors target CPU0; an unpinned SMP test may run elsewhere.
#[cfg(not(feature = "smp"))]
unittest::path_mod_test!(tests, "../ut-tests/irq.rs");

pub(crate) fn init() {
    // Keep the parent closed while replacing firmware's routing/source state.
    ecfg::set_lie(ecfg::read().lie() & !LineBasedInterrupt::HWI1);
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
    fn deferred_affinity(_desc: kirq::IrqDesc) -> Option<&'static kirq::IrqDeferredAffinityChip> {
        None
    }

    fn immediate_affinity(_desc: kirq::IrqDesc) -> Option<&'static kirq::IrqAffinityChip> {
        None
    }

    fn guest_interface() -> Option<kirq::GuestIrqInterface> {
        None
    }

    fn percpu_forwarding(_: kirq::IrqDesc) -> Option<&'static kirq::PerCpuForwardingChip> {
        None
    }

    fn percpu_nmi(_: kirq::IrqDesc) -> Option<&'static kirq::PerCpuNmiChip> {
        None
    }

    fn select_flow(desc: kirq::IrqDesc) -> Result<kirq::IrqFlow, kirq::IrqDescError> {
        select_flow(desc)
    }

    fn configure(desc: kirq::IrqDesc) -> Result<(), kirq::IrqDescError> {
        configure(desc)
    }

    fn enable(desc: kirq::IrqDesc, enabled: bool) {
        let irq = IrqType::new(desc.hwirq);
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
                    unmask_external(desc);
                } else {
                    mask_external(desc);
                }
            }
        }
    }

    /// # Safety
    /// The caller must uphold [`kirq::IntrManagerIf::dispatch_irq`]'s contract.
    unsafe fn dispatch_irq(irq: usize) -> Option<kirq::Virq> {
        let mut irq = IrqType::new(irq);
        if matches!(irq, IrqType::Io) {
            let Some(ex_irq) = claim_external(eiointc::claim_irq, eiointc::complete_irq) else {
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
            IrqType::Ex(_) => {}
        }
        // SAFETY: this entry has already acknowledged the source. The no-op
        // completion cookie is consumed under the caller's hardirq exclusion.
        let pending = unsafe { kirq::PendingIrq::new(kirq::IrqRef::Virq(irq.as_usize()), 0) };
        kirq::generic_handle_irq(pending)
    }

    /// # Safety
    /// The caller must uphold [`kirq::IntrManagerIf::dispatch_nmi`]'s contract.
    unsafe fn dispatch_nmi(_irq: usize) {}

    /// # Safety
    /// The caller must uphold [`kirq::IntrManagerIf::complete_irq`]'s contract.
    unsafe fn complete_irq(_completion_cookie: usize) {}

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
