// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{
    num::NonZeroU32,
    ptr::NonNull,
    sync::atomic::{AtomicPtr, Ordering},
};

use kplat::{
    cpu::id as this_cpu_id,
    interrupts::{Handler, HandlerTable, IntrManager, TargetCpu},
};
use kspin::SpinNoIrq;
use riscv::register::sie;
use riscv_plic::Plic;
use sbi_rt::HartMask;

use crate::config::{devices::PLIC_PADDR, plat::PHYS_VIRT_OFFSET};
pub(super) const INTC_IRQ_BASE: usize = 1 << (usize::BITS - 1);
#[allow(unused)]
pub(super) const S_SOFT: usize = INTC_IRQ_BASE + 1;
pub(super) const S_TIMER: usize = INTC_IRQ_BASE + 5;
pub(super) const S_EXT: usize = INTC_IRQ_BASE + 9;
static TIMER_HANDLER: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static IPI_HANDLER: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
pub const MAX_IRQ_COUNT: usize = 1024;
static IRQ_HANDLER_TABLE: HandlerTable<MAX_IRQ_COUNT> = HandlerTable::new();
static PLIC: SpinNoIrq<Plic> = SpinNoIrq::new(unsafe {
    Plic::new(NonNull::new((PHYS_VIRT_OFFSET + PLIC_PADDR) as *mut _).unwrap())
});
fn this_context() -> usize {
    let hart_id = this_cpu_id();
    hart_id * 2 + 1
}
pub(super) fn init_percpu() {
    unsafe {
        sie::set_ssoft();
        sie::set_stimer();
        sie::set_sext();
    }
    PLIC.lock().init_by_context(this_context());
}
macro_rules! with_cause {
    (
        $cause:expr, @S_TIMER =>
        $timer_op:expr, @S_SOFT =>
        $ipi_op:expr, @S_EXT =>
        $ext_op:expr, @EX_IRQ =>
        $plic_op:expr $(,)?
    ) => {
        match $cause {
            S_TIMER => $timer_op,
            S_SOFT => $ipi_op,
            S_EXT => $ext_op,
            other => {
                if other & INTC_IRQ_BASE == 0 {
                    $plic_op
                } else {
                    panic!("Unknown IRQ cause: {other}");
                }
            }
        }
    };
}
struct IntrManagerImpl;
#[impl_dev_interface]
impl IntrManager for IntrManagerImpl {
    fn enable(irq: usize, enabled: bool) {
        with_cause!(
            irq,
            @S_TIMER => {
                unsafe {
                    if enabled {
                        sie::set_stimer();
                    } else {
                        sie::clear_stimer();
                    }
                }
            },
            @S_SOFT => {},
            @S_EXT => {},
            @EX_IRQ => {
                let Some(irq) = NonZeroU32::new(irq as _) else {
                    return;
                };
                trace!("PLIC set enable: {irq} {enabled}");
                let mut plic = PLIC.lock();
                if enabled {
                    plic.set_priority(irq, 6);
                    plic.enable(irq, this_context());
                } else {
                    plic.disable(irq, this_context());
                }
            }
        );
    }

    fn reg_handler(irq: usize, handler: Handler) -> bool {
        with_cause!(
            irq,
            @S_TIMER => TIMER_HANDLER.compare_exchange(core::ptr::null_mut(), handler as *mut _, Ordering::AcqRel, Ordering::Acquire).is_ok(),
            @S_SOFT => IPI_HANDLER.compare_exchange(core::ptr::null_mut(), handler as *mut _, Ordering::AcqRel, Ordering::Acquire).is_ok(),
            @S_EXT => {
                warn!("External IRQ should be got from PLIC, not scause");
                false
            },
            @EX_IRQ => {
                if IRQ_HANDLER_TABLE.register_handler(irq, handler) {
                    Self::enable(irq, true);
                    true
                } else {
                    warn!("reg_handler handler for External IRQ {irq} failed");
                    false
                }
            }
        )
    }

    fn unreg_handler(irq: usize) -> Option<Handler> {
        with_cause!(
            irq,
            @S_TIMER => {
                let handler = TIMER_HANDLER.swap(core::ptr::null_mut(), Ordering::AcqRel);
                if !handler.is_null() {
                    Some(unsafe { core::mem::transmute::<*mut (), Handler>(handler) })
                } else {
                    None
                }
            },
            @S_SOFT => {
                let handler = IPI_HANDLER.swap(core::ptr::null_mut(), Ordering::AcqRel);
                if !handler.is_null() {
                    Some(unsafe { core::mem::transmute::<*mut (), Handler>(handler) })
                } else {
                    None
                }
            },
            @S_EXT => {
                warn!("External IRQ should be got from PLIC, not scause");
                None
            },
            @EX_IRQ => IRQ_HANDLER_TABLE.unregister_handler(irq).inspect(|_| Self::enable(irq, false))
        )
    }

    fn dispatch_irq(irq: usize) -> Option<usize> {
        with_cause!(
            irq,
            @S_TIMER => {
                trace!("IRQ: timer");
                let handler = TIMER_HANDLER.load(Ordering::Acquire);
                if !handler.is_null() {
                    unsafe { core::mem::transmute::<*mut (), Handler>(handler)() };
                }
                Some(irq)
            },
            @S_SOFT => {
                trace!("IRQ: IPI");
                let handler = IPI_HANDLER.load(Ordering::Acquire);
                if !handler.is_null() {
                    unsafe { core::mem::transmute::<*mut (), Handler>(handler)() };
                }
                Some(irq)
            },
            @S_EXT => {
                let mut plic = PLIC.lock();
                let Some(irq) = plic.claim(this_context()) else {
                    debug!("Spurious external IRQ");
                    return None;
                };
                trace!("IRQ: external {irq}");
                IRQ_HANDLER_TABLE.handle(irq.get() as usize);
                plic.complete(this_context(), irq);
                Some(irq.get() as usize)
            },
            @EX_IRQ => {
                unreachable!("Device-side IRQs should be dispatch_irqd by triggering the External Interrupt.");
            }
        )
    }

    fn notify_cpu(_interrupt_id: usize, target: TargetCpu) {
        match target {
            TargetCpu::Self_ => {
                let res = sbi_rt::send_ipi(HartMask::from_mask_base(1 << this_cpu_id(), 0));
                if res.is_err() {
                    warn!("notify_cpu failed: {res:?}");
                }
            }
            TargetCpu::Specific(cpu_id) => {
                let res = sbi_rt::send_ipi(HartMask::from_mask_base(1 << cpu_id, 0));
                if res.is_err() {
                    warn!("notify_cpu failed: {res:?}");
                }
            }
            TargetCpu::AllButSelf {
                me: cpu_id,
                total: cpu_num,
            } => {
                for i in 0..cpu_num {
                    if i != cpu_id {
                        let res = sbi_rt::send_ipi(HartMask::from_mask_base(1 << i, 0));
                        if res.is_err() {
                            warn!("notify_cpu_all_others failed: {res:?}");
                        }
                    }
                }
            }
        }
    }

    fn set_prio(_irq: usize, _priority: u8) {
        todo!()
    }

    fn save_disable() -> usize {
        todo!()
    }

    fn restore(_flag: usize) {
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
