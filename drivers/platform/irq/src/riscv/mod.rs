// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use core::{num::NonZeroU32, ptr::NonNull};

use fwnode::FwNode;
use kalloc_types::{AllocFlags, KBox};
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

mod firmware;
use firmware::{PlicConfig, PlicSources};

const PLIC_PADDR: usize = 0x0c00_0000;
const PLIC_MMIO_SIZE: usize = 0x0400_0000;
const PLIC_IOMAP_NAME: &str = "plic";
/// Well-known domain reserved for the first firmware-registered PLIC.
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
    /// Hardware-number namespace returned when this PLIC's firmware controller
    /// node was registered.
    domain: kirq::IrqDomainId,
    sources: PlicSources,
    state: SpinNoIrq<PlicState>,
}

struct PlicState {
    registers: Plic,
    // Controller-lifetime bindings; disable does not imply affinity migration.
    sources: KBox<[PlicSourceState]>,
}

#[derive(Clone, Copy, Default)]
struct PlicSourceState {
    target_context: Option<usize>,
    configured_priority: Option<u32>,
    is_masked: bool,
}

impl PlicSourceState {
    fn set_priority(
        &mut self,
        registers: &mut impl PlicPriorityRegisters,
        source: NonZeroU32,
        priority: u32,
    ) {
        self.configured_priority = Some(priority);
        registers.write_priority(source, if self.is_masked { 0 } else { priority });
    }

    fn mask(&mut self, registers: &mut impl PlicPriorityRegisters, source: NonZeroU32) {
        // Capture firmware policy only before the first policy write or mask.
        self.configured_priority
            .get_or_insert_with(|| registers.read_priority(source));
        self.is_masked = true;
        registers.write_priority(source, 0);
    }

    fn unmask(&mut self, registers: &mut impl PlicPriorityRegisters, source: NonZeroU32) {
        if !self.is_masked {
            return;
        }
        if let Some(priority) = self.configured_priority {
            registers.write_priority(source, priority);
        }
        self.is_masked = false;
    }

    fn set_enabled(
        &mut self,
        registers: &mut (impl PlicCompletion + PlicPriorityRegisters),
        context: usize,
        source: NonZeroU32,
        enabled: bool,
    ) {
        if enabled {
            self.set_priority(registers, source, 6);
        }
        set_plic_route(
            registers,
            &mut self.target_context,
            context,
            source,
            enabled,
        );
    }
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
        let mut state = self.state.lock();
        let PlicState { registers, sources } = &mut *state;
        sources[source.get() as usize].set_enabled(registers, this_context(), source, enabled);
    }

    fn set_priority(&self, id: usize, priority: u8) {
        let Some(source) = self.source(id) else {
            return;
        };
        let mut state = self.state.lock();
        let PlicState { registers, sources } = &mut *state;
        sources[source.get() as usize].set_priority(registers, source, priority.into());
    }

    fn mask(&self, id: usize) {
        let Some(source) = self.source(id) else {
            return;
        };
        let mut state = self.state.lock();
        let PlicState { registers, sources } = &mut *state;
        sources[source.get() as usize].mask(registers, source);
    }

    fn unmask(&self, id: usize) {
        let Some(source) = self.source(id) else {
            return;
        };
        let mut state = self.state.lock();
        let PlicState { registers, sources } = &mut *state;
        sources[source.get() as usize].unmask(registers, source);
    }

    fn complete(&self, id: usize) {
        // Completion also reads the source enable bit; validate before indexing.
        let Some(source) = self.source(id) else {
            return;
        };
        let mut state = self.state.lock();
        complete_plic_claim(&mut state.registers, this_context(), source);
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

/// Domain owned by the initialized PLIC, or the reserved primary before setup.
///
/// Admission may run while the legacy layout is still being configured, so the
/// well-known primary domain is the current instance until initialization
/// records the registered identifier.
fn current_plic_domain() -> kirq::IrqDomainId {
    PLIC.get()
        .map_or(PLIC_DOMAIN, |controller| controller.domain)
}

static PLIC_FASTEOI: kirq::FasteoiChip = kirq::FasteoiChip {
    resend: kirq::FasteoiResend::Software,
    mask: |desc| plic().mask(desc.hwirq),
    unmask: |desc| plic().unmask(desc.hwirq),
};

fn select_plic_flow(
    desc: kirq::IrqDesc,
    sources: Option<PlicSources>,
) -> Result<kirq::IrqFlow, kirq::IrqDescError> {
    use kirq::{IrqAffinity, IrqController, IrqFlags, IrqFlow, IrqPolarity, IrqTrigger};

    if desc.domain.is_none()
        && desc.controller == IrqController::Unknown
        && matches!(desc.hwirq, S_TIMER | S_SOFT)
    {
        return Ok(IrqFlow::PerCpu);
    }

    // External sources require a PLIC binding, not a bare hardware number. The
    // domain must belong to this initialized instance: this driver's register
    // operations use `desc.hwirq` against the global `plic()` controller, so
    // admitting another PLIC instance's domain would program the wrong device.
    if desc.domain.is_none() && desc.controller != IrqController::Plic {
        return Err(kirq::IrqDescError::UnsupportedFlow { hwirq: desc.hwirq });
    }
    if desc.domain != Some(current_plic_domain())
        || desc.controller != IrqController::Plic
        || !(desc.flags - (IrqFlags::SHARED | IrqFlags::WAKEUP_SOURCE)).is_empty()
        || desc.affinity != IrqAffinity::Any
        || !matches!(desc.trigger, IrqTrigger::Unknown(_) | IrqTrigger::LevelHigh)
        || !matches!(desc.polarity, IrqPolarity::Unknown | IrqPolarity::High)
        || sources
            .and_then(|sources| sources.source(desc.hwirq))
            .is_none()
    {
        return Err(kirq::IrqDescError::UnsupportedFlow { hwirq: desc.hwirq });
    }
    Ok(IrqFlow::Fasteoi(&PLIC_FASTEOI))
}

trait PlicPriorityRegisters {
    fn read_priority(&self, source: NonZeroU32) -> u32;
    fn write_priority(&mut self, source: NonZeroU32, priority: u32);
}

impl PlicPriorityRegisters for Plic {
    fn read_priority(&self, source: NonZeroU32) -> u32 {
        self.get_priority(source)
    }

    fn write_priority(&mut self, source: NonZeroU32, priority: u32) {
        self.set_priority(source, priority);
    }
}

trait PlicCompletion {
    fn is_enabled(&self, source: NonZeroU32, context: usize) -> bool;
    fn set_enabled(&mut self, source: NonZeroU32, context: usize, enabled: bool);
    fn complete(&mut self, context: usize, source: NonZeroU32);
}

// The controller lock serializes target selection and the enable-bit update.
// Prepared contexts start disabled, so disable-before-first-enable has no route.
fn set_plic_route(
    plic: &mut impl PlicCompletion,
    target: &mut Option<usize>,
    current_context: usize,
    source: NonZeroU32,
    enabled: bool,
) {
    if enabled {
        target.get_or_insert(current_context);
    }
    if let Some(context) = *target {
        plic.set_enabled(source, context, enabled);
    }
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

/// PLIC binding translator registered with the PLIC IRQ domain.
struct PlicXlate;

static PLIC_XLATE: PlicXlate = PlicXlate;

impl kirq::IrqDomainXlate for PlicXlate {
    fn translate(&self, params: &[u32]) -> Result<kirq::IrqHwSpec, kirq::IrqXlateError> {
        let &[source] = params else {
            return Err(kirq::IrqXlateError::Malformed);
        };
        if source == 0 || source > PLIC_MAX_SOURCE {
            return Err(kirq::IrqXlateError::InvalidHwirq);
        }
        Ok(kirq::IrqHwSpec {
            hwirq: source as usize,
            // The PLIC binding has no trigger cell; every source is level-high.
            trigger: kirq::IrqTrigger::LevelHigh,
        })
    }
}

/// Initializes the primary PLIC from the published firmware description.
///
/// `root` is the backend-neutral firmware root. Discovery walks it for the
/// first PLIC-compatible node and uses the translated register resource, so
/// this must run after allocator setup, like the other firmware-node
/// consumers.
pub fn init_primary(root: FwNode<'static>) {
    #[expect(
        clippy::panic,
        reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: invalid PLIC firmware \
                  metadata; interrupt initialization cannot safely continue"
    )]
    let selection = PlicConfig::from_fwnode(root)
        .unwrap_or_else(|err| panic!("invalid PLIC configuration: {err:?}"));
    // Preserve the legacy no-DT layout. This is a library capacity bound, not
    // discovery of the number of physically implemented sources.
    let (paddr, size, sources, controller) = match selection {
        Some(PlicConfig {
            paddr,
            size,
            sources,
            controller,
        }) => (paddr, size, sources, Some(controller)),
        None => (
            PhysAddr::from_usize(PLIC_PADDR),
            PLIC_MMIO_SIZE,
            PlicSources {
                last: PLIC_MAX_SOURCE,
            },
            None,
        ),
    };
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: PLIC source state is required \
                  before controller publication; early IRQ initialization has no recovery channel"
    )]
    let source_state = KBox::new_slice_with_flags(
        PlicSourceState::default(),
        sources.last as usize + 1,
        AllocFlags::NOWAIT,
    )
    .expect("failed to prepare PLIC source state");
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
    let domain = match controller {
        Some(controller) => {
            // This singleton boot path keeps the primary PLIC on the legacy
            // reserved domain. A future second controller instance must choose
            // `IrqDomainTarget::New` rather than inheriting this root.
            let registered = kirq::register_irq_domain(
                kirq::IrqDomainTarget::Existing(PLIC_DOMAIN),
                controller,
                &PLIC_XLATE,
            );
            #[expect(
                clippy::panic,
                reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: the \
                          firmware-described PLIC controller must be associated with its IRQ \
                          domain before consumers map interrupts; early IRQ initialization has no \
                          recovery channel"
            )]
            match registered {
                Ok(domain) => domain,
                Err(error) => panic!("failed to register PLIC IRQ domain: {error:?}"),
            }
        }
        // The legacy no-DT layout maps through the reserved primary domain,
        // which remains available without a firmware association.
        None => PLIC_DOMAIN,
    };
    PLIC.init_once(PlicController {
        domain,
        sources,
        state: SpinNoIrq::new(PlicState {
            registers: plic,
            sources: source_state,
        }),
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
    let mut state = controller.state.lock();
    let context = this_context();
    disable_plic_sources(&mut state.registers, context, controller.sources);
    state.registers.init_by_context(context);
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

unittest::path_mod_test!(tests, "../../ut-tests/riscv.rs");

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
        select_plic_flow(desc, PLIC.get().map(|controller| controller.sources))
    }

    fn prepare_local_irq_work() -> Option<fn()> {
        None
    }

    fn configure(_desc: kirq::IrqDesc) -> Result<(), kirq::IrqDescError> {
        // PLIC trigger wiring is fixed; select_flow already validates its metadata.
        Ok(())
    }

    fn enable(desc: kirq::IrqDesc, enabled: bool) {
        let irq = desc.hwirq;
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
            @S_SOFT => {
                if enabled {
                    // SAFETY: The caller pins this supervisor hart and retains
                    // IRQ exclusion; only its software-interrupt gate changes.
                    unsafe { sie::set_ssoft() };
                } else {
                    // SAFETY: The caller pins this supervisor hart with IRQs
                    // excluded, preventing local interrupt writers while this
                    // CSR operation clears only its S_SOFT enable bit.
                    unsafe { sie::clear_ssoft() };
                }
            },
            @S_EXT => {},
            @EX_IRQ => {
                plic().set_enabled(irq, enabled);
            }
        );
    }

    /// # Safety
    /// The caller must uphold [`kirq::IntrManagerIf::dispatch_irq`]'s contract.
    unsafe fn dispatch_irq(irq: usize) -> Option<kirq::Virq> {
        let pending_irq = with_cause!(
            irq,
            @S_TIMER => {
                trace!("IRQ: timer");
                // SAFETY: the captured timer cause requires no PLIC completion;
                // the caller retains CPU/IRQ exclusion until generic dispatch ends.
                Some(unsafe { kirq::PendingIrq::new(
                    kirq::IrqRef::Virq(irq),
                    PLIC_COMPLETE_SKIP,
                ) })
            },
            @S_SOFT => {
                trace!("IRQ: IPI");
                // SAFETY: the current trap cause is a pending supervisor
                // software interrupt, so clearing that CSR bit acknowledges it.
                unsafe { sip::clear_ssoft() };
                // SAFETY: this software interrupt was acknowledged above;
                // its no-op cookie stays on this CPU through generic dispatch.
                Some(unsafe { kirq::PendingIrq::new(
                    kirq::IrqRef::Virq(irq),
                    PLIC_COMPLETE_SKIP,
                ) })
            },
            @S_EXT => {
                let mut state = plic().state.lock();
                let Some(irq) = state.registers.claim(this_context()) else {
                    debug!("Spurious external IRQ");
                    return None;
                };
                trace!("IRQ: external {irq}");
                let hwirq = irq.get() as usize;
                // SAFETY: the PLIC claim is unique; generic dispatch consumes
                // it on this CPU before the caller releases entry exclusion.
                Some(unsafe { kirq::PendingIrq::new(
                    kirq::IrqRef::Domain(plic().domain, hwirq),
                    hwirq,
                ) })
            },
            @EX_IRQ => {
                unreachable!("Device-side IRQs should be handled by triggering the External Interrupt.");
            }
        );
        kirq::generic_handle_irq(pending_irq?)
    }

    /// # Safety
    /// The caller must uphold [`kirq::IntrManagerIf::dispatch_nmi`]'s contract.
    unsafe fn dispatch_nmi(_irq: usize) {}

    /// # Safety
    /// The caller must uphold [`kirq::IntrManagerIf::complete_irq`]'s contract.
    unsafe fn complete_irq(completion_cookie: usize) {
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
