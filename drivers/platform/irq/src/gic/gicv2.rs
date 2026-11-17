// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! GICv2 backend.

use arm_gic_driver::v2::*;
use kcpu_id_map::{LogicalCpuId, RawCpuId, raw_cpu_id};
use kirq::TargetCpu;
use kspin::SpinNoIrq;
use lazyinit::LazyInit;

use super::GicInitError;

mod affinity;
pub(super) use affinity::CHIP as AFFINITY;

static GIC: LazyInit<SpinNoIrq<Gic>> = LazyInit::new();
static TRAP_OP: LazyInit<TrapOp> = LazyInit::new();
/// Banked CPU-interface target routes, recorded before each CPU publishes
/// interrupt readiness. GICv2 target bits are not logical CPU numbers.
static CPU_TARGETS: CpuTargetMap = CpuTargetMap::new();
const PRIVATE_IRQ_COUNT: u32 = 32;

kcpu_slot::cpu_slot_cell! {
    static FORWARDED_PPIS: u32 = 0;
}

pub(super) static FORWARDING: kirq::PerCpuForwardingChip = kirq::PerCpuForwardingChip {
    set_state: set_forwarding_state,
};

/// Applies a validated CPU-local forwarding transition.
///
/// # Safety
/// The caller must uphold `PerCpuForwardingChip::set_state`'s source-lifetime,
/// IRQ exclusion and producer-quiescence requirements on the owning CPU.
unsafe fn set_forwarding_state(desc: kirq::IrqDesc, state: kirq::PerCpuForwardingState) {
    debug_assert!(!karch::local_irq_enabled());
    debug_assert!(!kirq::context::is_in_nmi());
    if !super::is_gic_domain(desc.domain)
        || !(16..32).contains(&desc.hwirq)
        || super::select_gic_flow(desc, super::STANDARD_INTID_LIMIT.get().copied())
            != Ok(kirq::IrqFlow::PerCpu)
    {
        return;
    }
    // SAFETY: capability selection admitted this GIC's PPI range 16..32.
    let intid = unsafe { IntId::raw(desc.hwirq as u32) };
    let gic = GIC.lock();
    let bit = 1u32 << desc.hwirq;
    let previous = FORWARDED_PPIS.load_current();
    match state {
        kirq::PerCpuForwardingState::Host => {
            gic.set_active(intid, false);
            FORWARDED_PPIS.store_current(previous & !bit);
        }
        kirq::PerCpuForwardingState::Guest { is_active } => {
            gic.set_active(intid, is_active);
            FORWARDED_PPIS.store_current(previous | bit);
        }
    }
}

unittest::path_mod_test!(config_tests, "../../ut-tests/gicv2_config.rs");
unittest::path_mod_test!(target_tests, "../../ut-tests/gicv2_target.rs");
unittest::path_mod_test!(
    affinity_delivery_tests,
    "../../ut-tests/gicv2_affinity_delivery.rs"
);

#[cfg(unittest)]
pub(super) fn pend_local_ppi_for_test(irq: usize) {
    // SAFETY: the parent admitted only the banked PPI range 16..32.
    let intid = unsafe { IntId::raw(irq as u32) };
    GIC.lock().set_pending(intid, true);
}

#[cfg(unittest)]
pub(super) fn local_ppi_state_for_test(irq: usize) -> (bool, bool, bool) {
    let gic = GIC.lock();
    // SAFETY: the parent validates the PPI range before calling this helper.
    let intid = unsafe { IntId::raw(irq as u32) };
    (
        gic.is_irq_enable(intid),
        gic.is_active(intid),
        FORWARDED_PPIS.load_current() & (1 << irq) != 0,
    )
}

/// Configures an admitted source without changing caller-owned priority.
///
/// Runs without sleeping in task or IRQ context, never NMI context or while
/// holding GIC. Private sources refer to the executing CPU: the caller must
/// select that CPU before entry. The GIC guard excludes IRQs and migration
/// throughout the transaction. Callers must quiesce devices/handlers separately.
///
/// # Errors
/// Admission errors precede register access. Missing initialization or observed
/// mask/configuration/restore failure returns `ConfigurationFailed`. A failed
/// transaction requests masking and never restores enablement; this is neither
/// rollback nor proof of hardware quiescence or readable/writable registers.
pub(super) fn configure(desc: kirq::IrqDesc) -> Result<(), kirq::IrqDescError> {
    let config = checked_config(desc, super::STANDARD_INTID_LIMIT.get().copied())?;
    let gic = GIC
        .get()
        .ok_or(kirq::IrqDescError::ConfigurationFailed { hwirq: desc.hwirq })?;
    if let Some((intid, trigger)) = config {
        let mut gic = gic.lock();
        configure_locked(&mut *gic, intid, trigger)?;
    }
    Ok(())
}

fn checked_config(
    desc: kirq::IrqDesc,
    limit: Option<usize>,
) -> Result<Option<(IntId, Trigger)>, kirq::IrqDescError> {
    super::select_gic_flow(desc, limit)?;
    // Admission deliberately permits undeclared legacy identities. Still bound
    // every register index here before narrowing it to the vendor ID type.
    if limit.is_none_or(|limit| desc.hwirq >= limit.min(1020)) {
        return Err(kirq::IrqDescError::UnsupportedFlow { hwirq: desc.hwirq });
    }
    let trigger = match desc.trigger {
        kirq::IrqTrigger::Unknown(_) => return Ok(None),
        kirq::IrqTrigger::EdgeRising => Trigger::Edge,
        kirq::IrqTrigger::EdgeFalling if desc.hwirq >= 16 => Trigger::Edge,
        kirq::IrqTrigger::LevelHigh | kirq::IrqTrigger::LevelLow if desc.hwirq >= 16 => {
            Trigger::Level
        }
        _ => return Err(kirq::IrqDescError::UnsupportedFlow { hwirq: desc.hwirq }),
    };
    // SGIs are fixed rising-edge and their enable bits may be RAO/WI. Linux
    // gic_set_type accepts this type without attempting a register transaction.
    if desc.hwirq < 16 {
        return Ok(None);
    }
    // SAFETY: the bound above restricts hwirq to implemented standard INTIDs
    // below 1020, excluding special IDs and truncation before vendor indexing.
    Ok(Some((unsafe { IntId::raw(desc.hwirq as u32) }, trigger)))
}

// This private interface lets deterministic tests exercise the production
// transaction without fabricating MMIO or changing the vendor register layer.
trait ConfigRegisters {
    fn get_cfg(&mut self, intid: IntId) -> Trigger;
    fn set_cfg(&mut self, intid: IntId, trigger: Trigger);
    fn is_irq_enable(&mut self, intid: IntId) -> bool;
    fn set_irq_enable(&mut self, intid: IntId, enabled: bool);
}

impl ConfigRegisters for Gic {
    fn get_cfg(&mut self, intid: IntId) -> Trigger {
        Gic::get_cfg(self, intid)
    }

    fn set_cfg(&mut self, intid: IntId, trigger: Trigger) {
        Gic::set_cfg(self, intid, trigger);
    }

    fn is_irq_enable(&mut self, intid: IntId) -> bool {
        Gic::is_irq_enable(self, intid)
    }

    fn set_irq_enable(&mut self, intid: IntId, enabled: bool) {
        Gic::set_irq_enable(self, intid, enabled);
    }
}

/// Requires one GIC guard across every access, including enable restoration.
fn configure_locked(
    registers: &mut impl ConfigRegisters,
    intid: IntId,
    trigger: Trigger,
) -> Result<(), kirq::IrqDescError> {
    // Fixed PPIs can accept an already matching type without writes. Unlike
    // Linux gic_set_type, a PPI mismatch is not downgraded to success here.
    if registers.get_cfg(intid) == trigger {
        return Ok(());
    }
    let was_enabled = registers.is_irq_enable(intid);
    if was_enabled {
        registers.set_irq_enable(intid, false);
    }
    if !was_enabled || !registers.is_irq_enable(intid) {
        registers.set_cfg(intid, trigger);
        if registers.get_cfg(intid) == trigger {
            if !was_enabled {
                return Ok(());
            }
            registers.set_irq_enable(intid, true);
            if registers.is_irq_enable(intid) {
                return Ok(());
            }
        }
    }
    // Never restore on failure, including a failed restore readback. A disable
    // request cannot establish quiescence if hardware ignores it or reads RAZ.
    registers.set_irq_enable(intid, false);
    Err(kirq::IrqDescError::ConfigurationFailed {
        hwirq: intid.to_u32() as usize,
    })
}

/// Looks up the target capability recorded for one logical CPU.
///
/// The routing API speaks logical CPU identities. Existing CPU-map ownership
/// supplies the stable hardware identity that the GICv2 target map is keyed by;
/// this function never treats a logical index as a target bit.
fn target_for_logical_cpu(
    logical_cpu: LogicalCpuId,
    targets: &CpuTargetMap,
    hardware_cpu_id: impl FnOnce(LogicalCpuId) -> Option<RawCpuId>,
) -> Option<CpuInterfaceTarget> {
    let hardware_cpu_id = hardware_cpu_id(logical_cpu)?.as_usize();
    targets.cpu_interface_target_for_hardware_cpu(hardware_cpu_id)
}

/// Records one CPU-interface association without overwriting an established one.
fn record_cpu_target(
    targets: &CpuTargetMap,
    logical_cpu: LogicalCpuId,
    hardware_cpu_id: usize,
    target: CpuInterfaceTarget,
) -> Result<(), GicInitError> {
    targets
        .record_cpu_interface_target(logical_cpu.as_usize(), hardware_cpu_id, target)
        .map_err(|error| match error {
            CpuTargetMapError::InvalidLogicalCpu => GicInitError::LogicalCpuOutOfRange,
            CpuTargetMapError::InvalidTarget => GicInitError::InvalidCpuTarget,
            CpuTargetMapError::ConflictingRoute => GicInitError::ConflictingCpuTarget,
        })
}

fn record_current_cpu_target(target: CpuInterfaceTarget) -> Result<(), GicInitError> {
    let logical_cpu = kplat::cpu::id();
    let hardware_cpu_id = raw_cpu_id(logical_cpu)
        .ok_or(GicInitError::MissingHardwareCpuId)?
        .as_usize();
    record_cpu_target(&CPU_TARGETS, logical_cpu, hardware_cpu_id, target)
}

pub(super) fn init(
    gicd_base: memaddr::VirtAddr,
    gicc_base: memaddr::VirtAddr,
) -> Result<(), GicInitError> {
    if GIC.get().is_some() {
        return Ok(());
    }
    info!("Initialize GICv2...");
    let gicd_base = VirtAddr::new(gicd_base.into());
    let gicc_base = VirtAddr::new(gicc_base.into());
    // SAFETY: both distributor and CPU interface bases come from platform
    // discovery and point at the mapped GICv2 MMIO frames for this machine.
    let mut gic = unsafe { Gic::new(gicd_base, gicc_base, None) };
    gic.set_cpu_target_map(&CPU_TARGETS);
    // Discovery precedes the vendor's distributor reset; a failure therefore
    // leaves the controller untouched. The boot target is seeded here so the
    // published controller always has its boot CPU association.
    let boot_target = gic
        .try_init()
        .map_err(|_| GicInitError::BootCpuTargetDiscovery)?;
    record_current_cpu_target(boot_target)?;
    super::remember_standard_intid_limit(gic.typer_raw());
    GIC.init_once(SpinNoIrq::new(gic));
    let cpu = GIC.lock().cpu_interface();
    TRAP_OP.init_once(cpu.trap_operations());
    Ok(())
}

pub(super) fn init_current_cpu() -> Result<(), GicInitError> {
    debug!("Initialize GICv2 CPU Interface...");
    let gic = GIC.get().ok_or(GicInitError::NotInitialized)?;
    // Configuration starts with local IRQ delivery masked and before runtime
    // publishes this CPU as interrupt-ready. Discovery and registration thus
    // complete before any affinity candidate can name this CPU.
    let target = {
        let gic = gic.lock();
        gic.cpu_interface()
            .discover_target()
            .map_err(|_| GicInitError::CurrentCpuTargetDiscovery)?
    };
    record_current_cpu_target(target)?;
    let gic = gic.lock();
    // These Distributor registers are banked: global initialization on the
    // primary cannot reset an AP's private state. Match Linux gic_cpu_config
    // before opening the CPU interface. GICv2 SGI enable bits can be RAO/WI;
    // CPU IRQ masking, not a disabled SGI readback, protects this boot phase.
    for irq in 0..PRIVATE_IRQ_COUNT {
        // SAFETY: 0..32 is the architected SGI/PPI interrupt-ID range.
        let intid = unsafe { IntId::raw(irq) };
        gic.set_active(intid, false);
        gic.set_irq_enable(intid, false);
    }
    let mut cpu = gic.cpu_interface();
    drop(gic);
    // The owned register handle also resets this CPU's SGI/PPI priorities
    // to 0xa0. It neither borrows the GIC lock nor touches shared SPI state.
    cpu.init_current_cpu();
    cpu.set_eoi_mode_ns(true);
    Ok(())
}

pub(super) fn set_trigger(interrupt_id: usize, edge: bool) {
    trace!("GICv2 set trigger: {interrupt_id} {edge}");
    // SAFETY: callers pass a hardware IRQ number understood by this GIC
    // instance; `IntId::raw` only wraps that validated numeric identifier.
    let intid = unsafe { IntId::raw(interrupt_id as u32) };
    let cfg = if edge { Trigger::Edge } else { Trigger::Level };
    GIC.lock().set_cfg(intid, cfg);
}

pub(super) fn enable(irq: usize, enabled: bool) {
    trace!("GICv2 set enable: {irq} {enabled}");
    // SAFETY: callers pass a hardware IRQ number understood by this GIC
    // instance; `IntId::raw` only wraps that validated numeric identifier.
    let intid = unsafe { IntId::raw(irq as u32) };
    let mut gic = GIC.lock();
    if intid.is_private() {
        gic.set_irq_enable(intid, enabled);
    } else {
        let _is_complete = super::spi::set_enabled(&mut *gic, intid, enabled);
    }
}

impl super::spi::Registers for Gic {
    fn set_enabled(&mut self, intid: IntId, enabled: bool) {
        Gic::set_irq_enable(self, intid, enabled);
    }

    fn wait_for_disable(&mut self) -> bool {
        true
    }

    fn set_pending(&mut self, intid: IntId) {
        Gic::set_pending(self, intid, true);
    }
}

pub(super) fn retrigger_spi(hwirq: usize) {
    // SAFETY: only the admitted ordinary SPI flow calls this operation, using
    // its original standard INTID, bounded by the discovered Distributor limit.
    let intid = unsafe { IntId::raw(hwirq as u32) };
    super::spi::set_pending(&mut *GIC.lock(), intid);
}

pub(super) fn set_prio(irq: usize, priority: u8) {
    // SAFETY: callers pass a hardware IRQ number understood by this GIC
    // instance; `IntId::raw` only wraps that validated numeric identifier.
    let intid = unsafe { IntId::raw(irq as u32) };
    let gic = GIC.lock();
    gic.set_priority(intid, priority);
}

/// IRQ path: ack + early EOI. GICv2 has no NMI classification.
/// Mirrors `__gic_handle_irq_from_irqson`.
/// # Safety
/// The caller must uphold [`kirq::IntrManagerIf::dispatch_irq`]'s contract.
pub(super) unsafe fn dispatch_irq_from_irqson() -> Option<kirq::Virq> {
    let (hwirq, completion_cookie) = claim_irq()?;
    // SAFETY: claim_irq acknowledged this unique source; the caller retains
    // hardirq context and CPU/IRQ exclusion through dispatch and completion.
    let pending = unsafe {
        kirq::PendingIrq::new(
            kirq::IrqRef::Domain(super::gic_domain(), hwirq),
            completion_cookie,
        )
    };
    kirq::generic_handle_irq(pending)
}

fn claim_irq() -> Option<(usize, usize)> {
    let ack = TRAP_OP.ack();
    if ack.is_special() {
        return None;
    }

    let irq = match ack {
        Ack::Other(intid) => intid,
        Ack::SGI { intid, cpu_id: _ } => intid,
    }
    .to_u32() as usize;
    trace!("IRQ: {ack:?}");

    // GIC non-secure mode requires an early EOI to lower the running priority,
    // while the actual deactivation is deferred to `complete_irq`.
    TRAP_OP.eoi(ack);
    // Order the acknowledged interrupt before the handler.
    aarch64_cpu::asm::barrier::isb(aarch64_cpu::asm::barrier::SY);
    Some((irq, u32::from(ack) as usize))
}

/// NMI path: ack the interrupt for completion.
///
/// GICv2 has **no pseudo‑NMI support**: PMR masking cannot promote an IRQ
/// into a non‑maskable interrupt, so this path is unreachable in a correct
/// build (the platform rejects NMI on GICv2).  The interrupt is still
/// acknowledged and completed so the GIC does not keep it pending.
pub(super) fn dispatch_irq_from_irqsoff() -> Option<(usize, usize)> {
    claim_irq()
}

fn is_deactivation_delegated(ack: Ack) -> bool {
    let hwirq = match ack {
        Ack::Other(intid) => intid,
        Ack::SGI { intid, cpu_id: _ } => intid,
    }
    .to_u32() as usize;

    // Delegation is local residency state, never inferred from a build option
    // or INTID alone. No task handoff may occur before this claim completes.
    hwirq < 32 && FORWARDED_PPIS.load_current() & (1u32 << hwirq) != 0
}

pub(super) fn complete_irq(completion_cookie: usize) {
    let ack = Ack::from(completion_cookie as u32);
    if is_deactivation_delegated(ack) {
        return;
    }
    TRAP_OP.dir(ack);
}

pub(super) fn notify_cpu(interrupt_id: usize, target: TargetCpu) {
    // `dmb ishst` orders the caller's prior Normal-memory stores before the
    // SGI MMIO write, which is required for cross-CPU publish-before-notify
    // semantics.
    aarch64_cpu::asm::barrier::dmb(aarch64_cpu::asm::barrier::ISHST);
    match target {
        TargetCpu::Self_ => {
            GIC.lock()
                .send_sgi(IntId::sgi(interrupt_id as u32), SGITarget::Current);
        }
        TargetCpu::Specific(cpu_id) => {
            let target_list = TargetList::new(core::iter::once(cpu_id));
            GIC.lock().send_sgi(
                IntId::sgi(interrupt_id as u32),
                SGITarget::TargetList(target_list),
            );
        }
        TargetCpu::AllButSelf { .. } => {
            GIC.lock()
                .send_sgi(IntId::sgi(interrupt_id as u32), SGITarget::AllOther);
        }
    }
}
