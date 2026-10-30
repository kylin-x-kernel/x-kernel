// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{num::NonZeroU32, ptr::NonNull};

use kcpu_id_map::{LogicalCpuId, raw_cpu_id};
use kirq::TargetCpu;
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
pub const PLIC_DOMAIN: kirq::IrqDomainId = kirq::PLIC_ROOT_DOMAIN;
pub const INTC_IRQ_BASE: usize = 1 << (usize::BITS - 1);

pub const S_SOFT: usize = INTC_IRQ_BASE + 1;
pub const S_TIMER: usize = INTC_IRQ_BASE + 5;
pub const S_EXT: usize = INTC_IRQ_BASE + 9;

/// Cookie sentinel: no PLIC completion needed (timer / IPI).
/// PLIC interrupt numbers start at 1, so 0 is safe as a sentinel.
const PLIC_COMPLETE_SKIP: usize = 0;
static PLIC: LazyInit<PlicController> = LazyInit::new();
// riscv_plic 0.2.0 indexes a 1024-entry priority array; entry zero is reserved.
const PLIC_MAX_SOURCE: u32 = 1023;

struct PlicController {
    sources: PlicSources,
    registers: SpinNoIrq<Plic>,
}

impl PlicController {
    fn source(&self, id: usize) -> Option<NonZeroU32> {
        let source = self.sources.source(id);
        if source.is_none() {
            warn!("PLIC source {id} is outside the initialized source range");
        }
        source
    }

    fn set_enabled(&self, id: usize, enabled: bool) {
        let Some(source) = self.source(id) else {
            return;
        };
        trace!("PLIC set enable: {source} {enabled}");
        let mut registers = self.registers.lock();
        if enabled {
            registers.set_priority(source, 6);
            registers.enable(source, this_context());
        } else {
            registers.disable(source, this_context());
        }
    }

    fn set_priority(&self, id: usize, priority: u8) {
        let Some(source) = self.source(id) else {
            return;
        };
        self.registers.lock().set_priority(source, priority.into());
    }

    fn complete(&self, id: usize) {
        // Completion also reads the source enable bit; validate before indexing.
        let Some(source) = self.source(id) else {
            return;
        };
        let mut registers = self.registers.lock();
        complete_plic_claim(&mut *registers, this_context(), source);
    }
}

#[derive(Clone, Copy)]
struct PlicSources {
    last: u32,
}

impl PlicSources {
    fn from_property(value: Option<&[u8]>) -> Result<Self, &'static str> {
        let bytes: [u8; 4] = value
            .ok_or("PLIC node is missing riscv,ndev")?
            .try_into()
            .map_err(|_| "PLIC riscv,ndev must contain exactly one u32")?;
        let last = u32::from_be_bytes(bytes);
        if last == 0 || last > PLIC_MAX_SOURCE {
            return Err("PLIC riscv,ndev exceeds supported source range 1..=1023");
        }
        Ok(Self { last })
    }

    fn source(self, id: usize) -> Option<NonZeroU32> {
        let id = u32::try_from(id).ok()?;
        (id <= self.last).then(|| NonZeroU32::new(id)).flatten()
    }
}

pub const fn plic_irq_desc(hwirq: usize) -> kirq::IrqDesc {
    kirq::plic_irq_desc(hwirq)
}

fn plic() -> &'static PlicController {
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Primary interrupt initialization installs \
                  the permanent PLIC before per-CPU setup or interrupt dispatch"
    )]
    PLIC.get().expect("PLIC is not initialized")
}

trait PlicCompletion {
    fn is_enabled(&self, source: NonZeroU32, context: usize) -> bool;
    fn set_enabled(&mut self, source: NonZeroU32, context: usize, enabled: bool);
    fn complete(&mut self, context: usize, source: NonZeroU32);
}

impl PlicCompletion for Plic {
    fn is_enabled(&self, source: NonZeroU32, context: usize) -> bool {
        Plic::is_enabled(self, source, context)
    }

    fn set_enabled(&mut self, source: NonZeroU32, context: usize, enabled: bool) {
        if enabled {
            self.enable(source, context);
        } else {
            self.disable(source, context);
        }
    }

    fn complete(&mut self, context: usize, source: NonZeroU32) {
        Plic::complete(self, context, source);
    }
}

// The caller holds the PLIC lock across this entire transaction, including
// selection of the claiming hart's context.
fn complete_plic_claim(plic: &mut impl PlicCompletion, context: usize, source: NonZeroU32) {
    // PLIC specification, Interrupt Completion: a target ignores completion
    // when this source is disabled. Match Linux plic_irq_eoi without changing
    // KIRQ's logical disable state or the source priority.
    let was_enabled = plic.is_enabled(source, context);
    if !was_enabled {
        plic.set_enabled(source, context, true);
    }
    plic.complete(context, source);
    if !was_enabled {
        plic.set_enabled(source, context, false);
    }
}

// Match Linux's PLIC context preparation: firmware enable bits must not create
// unowned routes when local delivery is activated. Source priority is global
// and must not be reset while preparing another CPU's context.
fn disable_plic_sources(plic: &mut impl PlicCompletion, context: usize, sources: PlicSources) {
    for source in (1..=sources.last).filter_map(NonZeroU32::new) {
        plic.set_enabled(source, context, false);
    }
}

fn plic_region_from_device_tree() -> Result<Option<(PhysAddr, usize, PlicSources)>, &'static str> {
    for compatible in ["sifive,plic-1.0.0", "riscv,plic0"] {
        let Some(node) = of::find_compatible(compatible) else {
            continue;
        };
        let sources = PlicSources::from_property(node.property("riscv,ndev").map(|p| p.value))?;
        let region = node
            .reg()
            .and_then(|mut regs| regs.next())
            .ok_or("PLIC node is missing its register region")?;
        return Ok(Some((
            PhysAddr::from_usize(region.starting_address as usize),
            region.size.max(0x1000),
            sources,
        )));
    }
    Ok(None)
}

pub fn init_primary() {
    #[expect(
        clippy::panic,
        reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: invalid PLIC firmware \
                  metadata; interrupt initialization cannot safely continue"
    )]
    let (paddr, size, sources) = plic_region_from_device_tree()
        .unwrap_or_else(|err| panic!("invalid PLIC configuration: {err}"))
        // Preserve the legacy no-DT layout. This is a library capacity bound,
        // not discovery of the number of physically implemented sources.
        .unwrap_or((
            PhysAddr::from_usize(PLIC_PADDR),
            PLIC_MMIO_SIZE,
            PlicSources {
                last: PLIC_MAX_SOURCE,
            },
        ));
    #[expect(
        clippy::panic,
        reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: the PLIC register window \
                  cannot be mapped; interrupt-controller initialization has no error return or \
                  recovery"
    )]
    let plic_base = iomap_device(paddr, size, PLIC_IOMAP_NAME)
        .unwrap_or_else(|err| panic!("failed to iomap PLIC: {err:?}"));
    #[expect(
        clippy::unwrap_used,
        reason = "CG-021-no-panic-on-reachable-failure: iomap_device succeeded and returned the \
                  live non-null PLIC MMIO mapping"
    )]
    // SAFETY: `plic_base` comes from a successful `iomap_device` of the PLIC
    // MMIO window, so the pointer is non-null and references the live register
    // block for the duration of the driver.
    let plic = unsafe { Plic::new(NonNull::new(plic_base.as_usize() as *mut _).unwrap()) };
    PLIC.init_once(PlicController {
        sources,
        registers: SpinNoIrq::new(plic),
    });
}

fn this_context() -> usize {
    let logical_cpu_id = this_cpu_id();
    #[expect(
        clippy::panic,
        reason = "CG-021-no-panic-on-reachable-failure: The current CPU's logical id was assigned \
                  from the present-CPU map at entry, so it has a raw hart id"
    )]
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
    // SAFETY: this fence publishes the caller's prior memory writes before the
    // SBI IPI request, so remote handlers cannot observe the interrupt before
    // its request state becomes visible.
    unsafe { core::arch::asm!("fence rw, rw", options(nomem, nostack)) };
    let res = sbi_rt::send_ipi(HartMask::from_mask_base(1, raw_hart_id));
    if res.is_err() {
        warn!("notify_cpu failed: {res:?}");
    }
}

/// Prepare this hart's PLIC context with all supported external sources disabled.
///
/// Called once per hart after global PLIC setup, with CPU IRQ delivery still
/// masked and before any source is enabled on this context. This is not a
/// runtime reset: invoking it after registration would disable live routes.
pub fn init_current_cpu_context() {
    let controller = plic();
    let mut registers = controller.registers.lock();
    let context = this_context();
    disable_plic_sources(&mut *registers, context, controller.sources);
    registers.init_by_context(context);
}

/// Enable this hart's software, timer and external interrupt sources.
///
/// Platform startup prepares the PLIC context separately and keeps global
/// supervisor IRQ delivery masked until runtime handlers are ready. This
/// function does not change the global SSTATUS.SIE gate or PLIC state.
pub fn activate_local_interrupts() {
    // SAFETY: Local interrupt initialization runs in supervisor mode; SSIE belongs to this hart.
    unsafe { sie::set_ssoft() };
    // SAFETY: The current hart owns its supervisor timer-enable CSR bit during local
    // initialization.
    unsafe { sie::set_stimer() };
    // SAFETY: The current hart owns its supervisor external-enable CSR bit during local
    // initialization.
    unsafe { sie::set_sext() };
}

unittest::path_mod_test!(tests, "../ut-tests/riscv.rs");

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

#[kiface::provide]
impl kirq::IntrManagerIf {
    fn prepare_local_irq_work() -> Option<fn()> {
        None
    }

    fn configure(_desc: kirq::IrqDesc) {}

    fn enable(irq: usize, enabled: bool) {
        with_cause!(
            irq,
            @S_TIMER => {
                // SAFETY: these CSR writes only toggle the current hart's
                // supervisor-timer enable bit.
                if enabled {
                    // SAFETY: Running in supervisor mode; this changes only
                    // the current hart's timer interrupt-enable bit.
                    unsafe { sie::set_stimer() };
                } else {
                    // SAFETY: Running in supervisor mode; this masks only the
                    // current hart's supervisor timer interrupt.
                    unsafe { sie::clear_stimer() };
                }
            },
            @S_SOFT => {},
            @S_EXT => {},
            @EX_IRQ => {
                plic().set_enabled(irq, enabled);
            }
        );
    }

    fn dispatch_irq(irq: usize) -> Option<kirq::Virq> {
        let pending_irq = with_cause!(
            irq,
            @S_TIMER => {
                trace!("IRQ: timer");
                Some(kirq::PendingIrq::new(
                    kirq::IrqRef::Virq(irq),
                    PLIC_COMPLETE_SKIP,
                ))
            },
            @S_SOFT => {
                trace!("IRQ: IPI");
                // SAFETY: the current trap cause is a pending supervisor
                // software interrupt, so clearing that CSR bit acknowledges it.
                unsafe { sip::clear_ssoft() };
                Some(kirq::PendingIrq::new(
                    kirq::IrqRef::Virq(irq),
                    PLIC_COMPLETE_SKIP,
                ))
            },
            @S_EXT => {
                let mut plic = plic().registers.lock();
                let Some(irq) = plic.claim(this_context()) else {
                    debug!("Spurious external IRQ");
                    return None;
                };
                trace!("IRQ: external {irq}");
                let hwirq = irq.get() as usize;
                Some(kirq::PendingIrq::new(
                    kirq::IrqRef::Domain(PLIC_DOMAIN, hwirq),
                    hwirq,
                ))
            },
            @EX_IRQ => {
                unreachable!("Device-side IRQs should be handled by triggering the External Interrupt.");
            }
        );
        kirq::generic_handle_irq(pending_irq?)
    }

    fn dispatch_nmi(_irq: usize) {}

    fn complete_irq(completion_cookie: usize) {
        if completion_cookie == PLIC_COMPLETE_SKIP {
            return;
        }
        plic().complete(completion_cookie);
    }

    fn notify_cpu(_interrupt_id: usize, target: TargetCpu) {
        match target {
            TargetCpu::Self_ => {
                let logical_cpu_id = this_cpu_id();
                #[expect(
                    clippy::panic,
                    reason = "CG-021-no-panic-on-reachable-failure: The current CPU's logical id \
                              was assigned from the present-CPU map at entry, so it has a raw \
                              hart id"
                )]
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

    fn set_prio(irq: usize, priority: u8) {
        with_cause!(
            irq,
            @S_TIMER => {},
            @S_SOFT => {},
            @S_EXT => {},
            @EX_IRQ => {
                plic().set_priority(irq, priority);
            }
        );
    }
}
