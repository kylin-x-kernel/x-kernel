// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{num::NonZeroU32, ptr::NonNull};

use kcpu_id_map::{LogicalCpuId, raw_cpu_id};
use khal::irq::TargetCpu;
use kplat::cpu::id as this_cpu_id;
use kspin::SpinNoIrq;
use lazyinit::LazyInit;
use memaddr::PhysAddr;
use memspace::iomap_device;
use riscv::register::{sie, sip};
use riscv_plic::Plic;
use sbi_rt::HartMask;

const PLIC_PADDR: usize = 0x0c00_0000;
const PLIC_MMIO_SIZE: usize = 0x0400_0000;
const PLIC_IOMAP_NAME: &str = "plic";
pub const PLIC_DOMAIN: khal::irq::IrqDomainId = khal::irq::PLIC_ROOT_DOMAIN;
pub const INTC_IRQ_BASE: usize = 1 << (usize::BITS - 1);
#[allow(unused)]
pub const S_SOFT: usize = INTC_IRQ_BASE + 1;
pub const S_TIMER: usize = INTC_IRQ_BASE + 5;
pub const S_EXT: usize = INTC_IRQ_BASE + 9;

/// Cookie sentinel: no PLIC completion needed (timer / IPI).
/// PLIC interrupt numbers start at 1, so 0 is safe as a sentinel.
const PLIC_COMPLETE_SKIP: usize = 0;
static PLIC: LazyInit<SpinNoIrq<Plic>> = LazyInit::new();

pub const fn plic_irq_desc(hwirq: usize) -> khal::irq::IrqDesc {
    khal::irq::plic_irq_desc(hwirq)
}

fn plic() -> &'static SpinNoIrq<Plic> {
    PLIC.get().expect("PLIC is not initialized")
}

fn plic_region_from_device_tree() -> Option<(PhysAddr, usize)> {
    for compatible in ["sifive,plic-1.0.0", "riscv,plic0"] {
        let Some(node) = of::find_compatible(compatible) else {
            continue;
        };
        let Some(region) = node.reg().and_then(|mut regs| regs.next()) else {
            continue;
        };
        return Some((
            PhysAddr::from_usize(region.starting_address as usize),
            region.size.max(0x1000),
        ));
    }
    None
}

pub fn init_primary() {
    let (paddr, size) = plic_region_from_device_tree()
        .unwrap_or((PhysAddr::from_usize(PLIC_PADDR), PLIC_MMIO_SIZE));
    let plic_base = iomap_device(paddr, size, PLIC_IOMAP_NAME)
        .unwrap_or_else(|err| panic!("failed to iomap PLIC: {err:?}"));
    let plic = unsafe { Plic::new(NonNull::new(plic_base.as_usize() as *mut _).unwrap()) };
    PLIC.init_once(SpinNoIrq::new(plic));
}

fn this_context() -> usize {
    let logical_cpu_id = this_cpu_id();
    let hart_id = raw_cpu_id(logical_cpu_id)
        .unwrap_or_else(|| {
            panic!(
                "missing raw CPU id mapping for current logical CPU {}",
                logical_cpu_id.as_usize()
            )
        })
        .as_usize();
    hart_id * 2 + 1
}

fn send_ipi_to_raw_hart(raw_hart_id: usize) {
    let res = sbi_rt::send_ipi(HartMask::from_mask_base(1, raw_hart_id));
    if res.is_err() {
        warn!("notify_cpu failed: {res:?}");
    }
}

pub fn init_current_cpu_context() {
    plic().lock().init_by_context(this_context());
}

pub fn init_percpu() {
    unsafe {
        sie::set_ssoft();
        sie::set_stimer();
        sie::set_sext();
    }
    init_current_cpu_context();
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

struct RiscvIrqIfImpl;

#[kplat::impl_dev_interface]
impl khal::irq::IntrManagerIf for RiscvIrqIfImpl {
    fn configure(_desc: khal::irq::IrqDesc) {}

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
                let mut plic = plic().lock();
                if enabled {
                    plic.set_priority(irq, 6);
                    plic.enable(irq, this_context());
                } else {
                    plic.disable(irq, this_context());
                }
            }
        );
    }

    fn dispatch_irq(irq: usize) -> Option<khal::irq::DispatchedIrq> {
        with_cause!(
            irq,
            @S_TIMER => {
                trace!("IRQ: timer");
                Some(khal::irq::DispatchedIrq::new(irq, PLIC_COMPLETE_SKIP))
            },
            @S_SOFT => {
                trace!("IRQ: IPI");
                unsafe { sip::clear_ssoft() };
                Some(khal::irq::DispatchedIrq::new(irq, PLIC_COMPLETE_SKIP))
            },
            @S_EXT => {
                let mut plic = plic().lock();
                let Some(irq) = plic.claim(this_context()) else {
                    debug!("Spurious external IRQ");
                    return None;
                };
                trace!("IRQ: external {irq}");
                let hwirq = irq.get() as usize;
                Some(khal::irq::DispatchedIrq::new(
                    khal::irq::resolve_hwirq(PLIC_DOMAIN, hwirq),
                    hwirq,
                ))
            },
            @EX_IRQ => {
                unreachable!("Device-side IRQs should be dispatch_irqd by triggering the External Interrupt.");
            }
        )
    }

    fn complete_irq(completion_cookie: usize) {
        if completion_cookie == PLIC_COMPLETE_SKIP {
            return;
        }
        // completion_cookie is a PLIC claim value (hwirq >= 1).
        // A zero or otherwise invalid cookie here indicates a
        // programming error in dispatch_irq.
        let Some(irq) = NonZeroU32::new(completion_cookie as _) else {
            warn!("PLIC complete_irq: bogus cookie {completion_cookie}");
            return;
        };
        plic().lock().complete(this_context(), irq);
    }

    fn notify_cpu(_interrupt_id: usize, target: TargetCpu) {
        match target {
            TargetCpu::Self_ => {
                let logical_cpu_id = this_cpu_id();
                let raw_cpu_id = raw_cpu_id(logical_cpu_id).unwrap_or_else(|| {
                    panic!(
                        "missing raw CPU id mapping for current logical CPU {}",
                        logical_cpu_id.as_usize()
                    )
                });
                send_ipi_to_raw_hart(raw_cpu_id.as_usize());
            }
            TargetCpu::Specific(logical_cpu_id) => {
                let Some(raw_cpu_id) = raw_cpu_id(LogicalCpuId::new(logical_cpu_id)) else {
                    warn!("RISC-V notify_cpu: missing raw CPU id for logical CPU {logical_cpu_id}");
                    return;
                };
                send_ipi_to_raw_hart(raw_cpu_id.as_usize());
            }
            TargetCpu::AllButSelf {
                me: local_logical_cpu_id,
                total: cpu_num,
            } => {
                for logical_cpu_id in 0..cpu_num {
                    if logical_cpu_id != local_logical_cpu_id {
                        let Some(raw_cpu_id) = raw_cpu_id(LogicalCpuId::new(logical_cpu_id)) else {
                            warn!(
                                "RISC-V notify_cpu: missing raw CPU id for logical CPU \
                                 {logical_cpu_id}"
                            );
                            continue;
                        };
                        send_ipi_to_raw_hart(raw_cpu_id.as_usize());
                    }
                }
            }
        }
    }

    fn set_prio(_irq: usize, _priority: u8) {
        with_cause!(
            _irq,
            @S_TIMER => {},
            @S_SOFT => {},
            @S_EXT => {},
            @EX_IRQ => {
                let Some(irq) = NonZeroU32::new(_irq as _) else {
                    return;
                };
                plic().lock().set_priority(irq, _priority.into());
            }
        );
    }
}
